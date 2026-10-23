/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Plays fragmented MP4 segments through a `MediaSource` attached to a
//! player, the way a page using Media Source Extensions would: an
//! initialization segment followed by media segments, appended one at a time
//! as the source buffer finishes each update, then end of stream.
//!
//! Usage: media_source <init segment> <media segment>... [--type <content type>]
//!
//! Prints the events as they come and the frames rendered, and exits with
//! an error if playback did not produce frames or did not reach the end.

extern crate servo_media;
extern crate servo_media_auto;

use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{env, fs, process};

use servo_base::generic_channel::GenericCallback;
use servo_media::player::context::{GlApi, GlContext, NativeDisplay, PlayerGLContext};
use servo_media::player::media_source::{
    EndOfStreamError, MediaSourceEvent, MediaSourceReadyState, SourceBufferEvent,
    SourceBufferReport,
};
use servo_media::player::video::{VideoFrame, VideoFrameRenderer};
use servo_media::player::{PlaybackState, PlayerEvent, StreamType};
use servo_media::{ClientContextId, ServoMedia};

const DEFAULT_TYPE: &str = "video/mp4; codecs=\"avc1.64001f, mp4a.40.2\"";

struct NoGLContext;

impl PlayerGLContext for NoGLContext {
    fn get_gl_context(&self) -> GlContext {
        GlContext::Unknown
    }

    fn get_native_display(&self) -> NativeDisplay {
        NativeDisplay::Unknown
    }

    fn get_gl_api(&self) -> GlApi {
        GlApi::None
    }
}

struct FrameCounter {
    frames: usize,
    size: (i32, i32),
}

impl VideoFrameRenderer for FrameCounter {
    fn render(&mut self, frame: VideoFrame) {
        self.frames += 1;
        self.size = (frame.get_width(), frame.get_height());
    }
}

fn wait_for<T: std::fmt::Debug>(
    receiver: &Receiver<T>,
    what: &str,
    timeout: Duration,
    mut matches: impl FnMut(&T) -> bool,
) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            eprintln!("Timed out waiting for {what}");
            process::exit(1);
        };
        match receiver.recv_timeout(left) {
            Ok(event) => {
                println!("  {event:?}");
                if matches(&event) {
                    return event;
                }
            },
            Err(_) => {
                eprintln!("Timed out waiting for {what}");
                process::exit(1);
            },
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut content_type = DEFAULT_TYPE.to_owned();
    let mut segments = vec![];
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if arg == "--type" {
            content_type = iter.next().expect("--type needs a value");
        } else {
            segments.push(arg);
        }
    }
    if segments.is_empty() {
        eprintln!("Usage: media_source <init segment> <media segment>... [--type <content type>]");
        process::exit(2);
    }

    ServoMedia::init::<servo_media_auto::Backend>();
    let servo_media = ServoMedia::get();
    if !servo_media.is_media_source_type_supported(&content_type) {
        eprintln!("Content type not supported by the backend: {content_type}");
        process::exit(1);
    }

    let (player_sender, player_events) = channel();
    let player_callback = GenericCallback::new(move |event: Result<PlayerEvent, _>| {
        if let Ok(event) = event {
            let _ = player_sender.send(event);
        }
    })
    .unwrap();
    let renderer = Arc::new(Mutex::new(FrameCounter {
        frames: 0,
        size: (0, 0),
    }));
    let player = servo_media.create_player(
        &ClientContextId::build(1, 1),
        StreamType::MediaSource,
        player_callback,
        Some(renderer.clone()),
        None,
        Box::new(NoGLContext),
    );

    let (source_sender, source_events) = channel();
    let source_callback = GenericCallback::new(move |event: Result<MediaSourceEvent, _>| {
        if let Ok(event) = event {
            let _ = source_sender.send(event);
        }
    })
    .unwrap();
    println!("Attaching media source");
    let source = player
        .lock()
        .unwrap()
        .attach_media_source(source_callback)
        .expect("Could not attach a media source");
    wait_for(
        &source_events,
        "sourceopen",
        Duration::from_secs(5),
        |event| *event == MediaSourceEvent::SourceOpen,
    );
    assert_eq!(source.ready_state(), MediaSourceReadyState::Open);

    let (buffer_sender, buffer_events) = channel();
    let buffer_callback = GenericCallback::new(move |event: Result<SourceBufferReport, _>| {
        if let Ok(event) = event {
            let _ = buffer_sender.send(event);
        }
    })
    .unwrap();
    let buffer = source
        .add_source_buffer(&content_type, buffer_callback)
        .expect("Could not add a source buffer");
    println!("Source buffer added for {}", buffer.content_type());

    for path in &segments {
        let data = fs::read(path).unwrap_or_else(|error| {
            eprintln!("Could not read {path}: {error}");
            process::exit(2);
        });
        println!("Appending {path} ({} bytes)", data.len());
        let operation = buffer.append(data).expect("Append refused");
        wait_for(
            &buffer_events,
            "updateend",
            Duration::from_secs(10),
            |report| report.operation == operation && report.event == SourceBufferEvent::UpdateEnd,
        );
        println!("  buffered at updateend: {:?}", buffer.buffered());
    }
    println!("End of stream");
    source
        .end_of_stream(EndOfStreamError::None)
        .expect("end_of_stream refused");
    wait_for(
        &source_events,
        "sourceended",
        Duration::from_secs(5),
        |event| *event == MediaSourceEvent::SourceEnded,
    );
    println!("Duration: {:?}", source.duration());

    println!("Playing");
    player.lock().unwrap().play().expect("play failed");
    let started = Instant::now();
    wait_for(
        &player_events,
        "end of stream",
        Duration::from_secs(120),
        |event| {
            matches!(event, PlayerEvent::EndOfStream | PlayerEvent::Error(_)) ||
                matches!(event, PlayerEvent::StateChanged(PlaybackState::Stopped))
        },
    );
    let renderer = renderer.lock().unwrap();
    println!(
        "Rendered {} frames of {}x{} in {:.1} s",
        renderer.frames,
        renderer.size.0,
        renderer.size.1,
        started.elapsed().as_secs_f64()
    );
    if renderer.frames == 0 {
        eprintln!("No frames were rendered");
        process::exit(1);
    }
}
