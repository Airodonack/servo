/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Console messages as the embedder receives them, with and without the devtools server.
//!
//! The devtools server makes each console call build a preview of its arguments for a
//! devtools client. These tests check that the preview stays bounded for object graphs
//! that are large through sharing or depth, and that the embedder's string does not depend
//! on whether the server runs.

mod common;

use std::rc::Rc;

use servo::{Preferences, WebViewBuilder};
use url::Url;

use crate::common::{ServoTest, WebViewDelegateImpl};

fn servo_test(devtools_server: bool) -> ServoTest {
    ServoTest::new_with_builder(|builder| {
        let mut preferences = Preferences::default();
        preferences.network_http_proxy_uri = String::new();
        preferences.network_https_proxy_uri = String::new();
        preferences.devtools_server_enabled = devtools_server;
        // Any free port: no client connects, the server only has to run.
        preferences.devtools_server_listen_address = "127.0.0.1:0".into();
        builder.preferences(preferences)
    })
}

/// Loads a page running `script`, waits for its `console.log("done")` and returns the
/// embedder's console messages before it.
fn console_messages(devtools_server: bool, script: &str) -> Vec<String> {
    let servo_test = servo_test(devtools_server);
    let delegate = Rc::new(WebViewDelegateImpl::default());
    let _webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(
            Url::parse(&format!(
                "data:text/html,<!doctype html><script>{script}; console.log('done');</script>"
            ))
            .unwrap(),
        )
        .build();

    let captured = delegate.clone();
    servo_test.spin(move || {
        !captured
            .console_messages
            .borrow()
            .iter()
            .any(|(_, message)| message == "done")
    });

    let messages = delegate.console_messages.borrow();
    messages
        .iter()
        .map(|(_, message)| message.clone())
        .take_while(|message| message != "done")
        .collect()
}

/// A graph of 31 objects, each holding the next one twice, has 2^30 paths. Logging it
/// must not walk every path.
const LOG_SHARED_GRAPH: &str = "let o = {leaf: true}; \
    for (let i = 0; i < 30; i++) o = {a: o, b: o}; \
    console.log(o); console.dir(o); console.table(o)";

/// A chain of 100,000 objects. Logging it must not recurse once per link.
const LOG_DEEP_CHAIN: &str = "let o = {end: true}; \
    for (let i = 0; i < 100000; i++) o = {next: o}; \
    console.log(o); console.dir(o)";

const DIR_NESTED_OBJECT: &str = "console.dir({a: {b: [1, 2]}, s: 'x', n: null})";
const DIR_NESTED_OBJECT_STRING: &str = "Object {a: Object {b: [1, 2]}, s: x, n: null}";

#[test]
fn test_console_log_of_shared_graph_with_devtools_server() {
    assert_eq!(console_messages(true, LOG_SHARED_GRAPH).len(), 3);
}

#[test]
fn test_console_log_of_shared_graph_without_devtools_server() {
    assert_eq!(console_messages(false, LOG_SHARED_GRAPH).len(), 3);
}

#[test]
fn test_console_log_of_deep_chain_with_devtools_server() {
    assert_eq!(console_messages(true, LOG_DEEP_CHAIN).len(), 2);
}

#[test]
fn test_console_dir_string_with_devtools_server() {
    assert_eq!(
        console_messages(true, DIR_NESTED_OBJECT),
        [DIR_NESTED_OBJECT_STRING]
    );
}

#[test]
fn test_console_dir_string_without_devtools_server() {
    assert_eq!(
        console_messages(false, DIR_NESTED_OBJECT),
        [DIR_NESTED_OBJECT_STRING]
    );
}
