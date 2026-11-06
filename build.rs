fn main() {
    // 仅 Windows 下嵌入图标与 asInvoker 清单。
    // gnu 工具链依赖 windres；若不可用仅告警跳过（无清单时系统默认即 asInvoker）。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // embed-resource 2.5：compile 返回 ()，内部失败会 panic；windres 缺失时仅告警跳过
        if std::panic::catch_unwind(|| {
            embed_resource::compile("assets/app.rc", embed_resource::NONE)
        })
        .is_err()
        {
            println!(
                "cargo:warning=windres 不可用，跳过图标/清单嵌入（无清单时系统默认即 asInvoker）"
            );
        }
    }
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/app.manifest");
    println!("cargo:rerun-if-changed=assets/icon.ico");
}
