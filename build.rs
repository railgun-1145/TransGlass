fn main() {
    embed_manifest::embed_manifest_file("transglass.manifest").expect("embed manifest");
    // exe 文件图标（资源管理器/快捷方式看到的那个）：icon.ico 由 icon.png 生成
    let _ = embed_resource::compile("app_icon.rc", embed_resource::NONE);
}
