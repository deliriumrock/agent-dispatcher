//! Сборочный сценарий: версия для показа людям.
//!
//! Третья цифра версии в `Cargo.toml` — номер сборки. Semver не допускает ведущих
//! нулей, поэтому в манифесте стоит `0.8.1`, а программа показывает `0.8.001`:
//! так номера одной ветки выравниваются и сравниваются на глаз.

use std::env::{self, VarError};

fn main() -> Result<(), VarError> {
    let major = env::var("CARGO_PKG_VERSION_MAJOR")?;
    let minor = env::var("CARGO_PKG_VERSION_MINOR")?;
    let build = env::var("CARGO_PKG_VERSION_PATCH")?;
    println!("cargo:rustc-env=DISP_VERSION={major}.{minor}.{build:0>3}");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    Ok(())
}
