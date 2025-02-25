/*
Copyright 2022 The Kuasar Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use std::fs;

use ttrpc_codegen::{Codegen, Customize, ProtobufCustomize};

fn main() {
    let protos = [
        "src/protos/sandbox.proto",
        "src/protos/github.com/containerd/containerd/api/services/ttrpc/events/v1/events.proto",
        "src/protos/github.com/containerd/containerd/protobuf/plugin/fieldpath.proto",
        "src/protos/google/protobuf/any.proto",
        "src/protos/google/protobuf/descriptor.proto",
        "src/protos/google/protobuf/empty.proto",
        "src/protos/google/protobuf/timestamp.proto",
        "src/protos/streaming.proto",
        "src/protos/containerd/types/transfer/data.proto",
    ];

    Codegen::new()
        .out_dir("src/api")
        .inputs(protos)
        .include("src/protos")
        .rust_protobuf()
        .customize(Customize {
            async_all: true,
            ..Customize::default()
        })
        .rust_protobuf_customize(
            ProtobufCustomize::default()
                .gen_mod_rs(false)
                .generate_accessors(true),
        )
        .run()
        .expect("Gen protos code failed");

    // Protobuf-rust 3.5.1 no longer generates the `#![allow(box_pointers)]` lint.
    // However, ttrpc-rust has not yet upgraded to protobuf-rust 3.5.1.
    // As a temporary measure, we are modifying the files to suppress the warning.
    remove_box_pointers("src/api").expect("Remove api box_pointer failed");
}

fn remove_box_pointers(dir: &str) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_file() {
            let content = fs::read_to_string(&path)?;
            let new_content = content.replace("#![allow(box_pointers)]", "");

            fs::write(path, new_content)?;
        }
    }
    Ok(())
}
