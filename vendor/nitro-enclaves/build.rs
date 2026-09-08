// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(host)");

    if Path::new("/dev/nitro-enclaves").exists() {
        println!("cargo:rustc-cfg=host");
    }
}
