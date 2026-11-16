fn main() {
    embuild::espidf::sysenv::output();
    // `board_xiao`: one of the Seeed Studio XIAO boards, which have one USB
    // port and no USB host role and bridge a UART instead.
    println!("cargo:rustc-check-cfg=cfg(board_xiao)");
    if std::env::var_os("CARGO_FEATURE_BOARD_XIAO_S3").is_some()
        || std::env::var_os("CARGO_FEATURE_BOARD_XIAO_C3").is_some()
    {
        println!("cargo:rustc-cfg=board_xiao");
    }
}
