//! Компилирует описание окна `ui/app.slint` в код Rust.

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    if let Err(err) = slint_build::compile_with_config("ui/app.slint", config) {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
