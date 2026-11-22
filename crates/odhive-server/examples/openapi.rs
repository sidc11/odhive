//! Prints the API's OpenAPI document (JSON) to stdout.
//!
//! `cargo run -q -p odhive-server --example openapi > web/src/lib/api/openapi.json`
#![forbid(unsafe_code)]

fn main() {
    let doc = odhive_server::api::openapi()
        .to_pretty_json()
        .expect("the OpenAPI document serializes");
    println!("{doc}");
}
