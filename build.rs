fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/easy-editor.ico");

    // exe 文件本身的图标：把 .ico 编进 PE 资源。winresource 只在显式设置时才写 manifest，
    // 所以这里只动图标，不碰 DPI/权限那些链接期默认值。
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/easy-editor.ico");
        // winresource 默认拿 package.name 填这两项，Windows 的对话框/属性页会显示成
        // 「easy-editor」，和品牌名对不上，这里改回来。
        res.set("ProductName", "EasyEditor");
        res.set("FileDescription", "EasyEditor");
        res.set("OriginalFilename", "EasyEditor.exe");
        if let Err(err) = res.compile() {
            println!("cargo:warning=没能把图标嵌进 exe（文件图标退回系统默认）：{err}");
        }
    }
}
