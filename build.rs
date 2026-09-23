// `jfd update` がどのリリースバイナリを取りに行くか決めるため、ターゲット名を埋め込む
fn main() {
    println!("cargo:rustc-env=JFD_TARGET={}", std::env::var("TARGET").unwrap());
}
