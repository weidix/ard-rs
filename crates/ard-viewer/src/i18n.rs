use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[serde(rename = "zh-CN")]
    SimplifiedChinese,
    #[serde(rename = "en")]
    English,
}

impl Language {
    pub const ALL: [Self; 2] = [Self::SimplifiedChinese, Self::English];

    pub fn from_code(code: &str) -> Self {
        match code {
            "en" | "en-US" | "en-GB" => Self::English,
            _ => Self::SimplifiedChinese,
        }
    }

    pub const fn code(self) -> &'static str {
        match self {
            Self::SimplifiedChinese => "zh-CN",
            Self::English => "en",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::SimplifiedChinese => "简体中文",
            Self::English => "English",
        }
    }

    pub fn tr(self, value: &str) -> &str {
        if self == Self::SimplifiedChinese {
            return value;
        }
        match value {
            "可连接" => "Available",
            "历史记录" => "History",
            "最近连接" => "Recently connected",
            "历史连接" => "Connection history",
            "选择记录可快速填写" => "Select an entry to fill the form",
            "搜索历史连接" => "Search connection history",
            "暂无历史连接" => "No connection history",
            "未找到匹配记录" => "No matching entries",
            "连接成功后会显示在这里" => "Successful connections appear here",
            "请尝试其他关键词" => "Try a different search",
            "连接" => "Connect",
            "复制地址" => "Copy address",
            "复制用户名" => "Copy username",
            "删除记录" => "Delete entry",
            "连接到远程设备" => "Connect to a remote device",
            "输入远程地址和凭据，密码由系统安全存储。" => {
                "Enter the remote address and credentials. Passwords are stored securely."
            }
            "远程地址" => "Remote address",
            "端口" => "Port",
            "默认 5900" => "Default 5900",
            "用户名" => "Username",
            "远程账户名" => "Remote account name",
            "已安全保存" => "Saved securely",
            "输入远程密码" => "Enter remote password",
            "密码" => "Password",
            "记住密码" => "Remember password",
            "添加到历史连接" => "Add to connection history",
            "连接参数" => "Connection options",
            "视频质量" => "Video quality",
            "远程分辨率" => "Remote resolution",
            "仅高性能模式下生效" => "Only applies in high-performance mode",
            "固定 2× 像素比例，仅高性能模式下生效" => {
                "Fixed 2× pixel ratio; only applies in high-performance mode"
            }
            "请选择受支持的固定分辨率" => "Select a supported fixed resolution",
            "请输入有效分辨率，例如 2560x1440 或 2560x1440,1920x1080" => {
                "Enter a valid resolution, such as 2560x1440 or 2560x1440,1920x1080"
            }
            "高性能编码" => "High-performance codecs",
            "帧率 (FPS)" => "Frame rate (FPS)",
            "画面插值" => "Display interpolation",
            "三次锐化采样" => "Bicubic sharp sampling",
            "启用" => "Enabled",
            "自动" => "Auto",
            "像素格式：服务器原生  ·  缩放：适应窗口  ·  自动重连：已启用" => {
                "Pixel format: Server native  ·  Scaling: Fit window  ·  Auto reconnect: On"
            }
            "密码使用应用本地加密凭据库保存，不写入明文配置文件。" => {
                "Passwords are stored in the app's encrypted credential vault, not in plain-text configuration."
            }
            "导出快捷方式" => "Export shortcuts",
            "加入历史" => "Add to history",
            "设置" => "Settings",
            "偏好设置" => "Preferences",
            "常规" => "General",
            "显示与性能" => "Display & performance",
            "按键映射" => "Key mapping",
            "安全" => "Security",
            "关于" => "About",
            "当前使用深色外观" => "Dark appearance is currently active",
            "当前使用浅色外观" => "Light appearance is currently active",
            "配置应用的通用行为。" => "Configure general application behavior.",
            "外观" => "Appearance",
            "主题模式" => "Theme",
            "语言" => "Language",
            "显示会话性能信息" => "Show session performance information",
            "输入控制" => "Input controls",
            "反转滚轮方向" => "Reverse scroll direction",
            "同时反转垂直和水平滚动方向。" => {
                "Reverse both vertical and horizontal scrolling."
            }
            "快捷按钮" => "Toolbar buttons",
            "选择会话工具栏中显示哪些快捷按钮。" => {
                "Choose which shortcuts appear in the session toolbar."
            }
            "截屏" => "Screenshot",
            "App 切换" => "App Switcher",
            "调度中心" => "Mission Control",
            "桌面" => "Desktop",
            "缩小" => "Zoom Out",
            "放大" => "Zoom In",
            "实际画面" => "Actual Size",
            "缩放至窗口大小" => "Fit to Window",
            "远程键盘" => "Remote Keyboard",
            "键鼠输入" => "Pointer & Keyboard",
            "剪贴板" => "Clipboard",
            "系统快捷键" => "System Shortcuts",
            "重置缩放" => "Reset Zoom",
            "配置本地按键如何发送到远程设备。" => {
                "Configure how local keys are sent to the remote device."
            }
            "预设" => "Preset",
            "macOS 默认" => "macOS Default",
            "Windows 默认" => "Windows Default",
            "Linux 默认" => "Linux Default",
            "复制预设" => "Duplicate preset",
            "重置" => "Reset",
            "本地按键" => "Local key",
            "远程动作" => "Remote action",
            "作用域" => "Scope",
            "添加常用映射" => "Add mapping",
            "拖动可调整优先级" => "Drag to change priority",
            "常用选项" => "Common options",
            "自动适配远程键盘布局" => "Adapt to remote keyboard layout",
            "在窗口获取焦点时屏蔽系统快捷键" => {
                "Block system shortcuts while the window is focused"
            }
            "设置远程画面的质量和性能。" => {
                "Configure remote display quality and performance."
            }
            "远程画面" => "Remote display",
            "管理本地凭据与连接数据。" => {
                "Manage local credentials and connection data."
            }
            "凭据存储" => "Credential storage",
            "在应用本地加密凭据库中保存当前设备密码" => {
                "Save the current device password in the app's encrypted vault"
            }
            "密码保存在独立的 AES-256-GCM 加密文件中，不会写入明文配置。" => {
                "Passwords are stored in a separate AES-256-GCM encrypted file and never written to plain-text configuration."
            }
            "Apple Remote Desktop 原生 Rust 客户端。" => {
                "A native Rust client for Apple Remote Desktop."
            }
            "支持 ARD 认证、加密传输、MVS GPU 解码、键鼠输入、剪贴板与自动重连。" => {
                "Supports ARD authentication, encrypted transport, MVS GPU decoding, keyboard and mouse input, clipboard sync, and automatic reconnection."
            }
            "许可证：MIT OR Apache-2.0" => "License: MIT OR Apache-2.0",
            "黑白" => "Black & White",
            "灰度" => "Grayscale",
            "16位颜色" => "16-bit Color",
            "高性能 AVC" => "High performance (AVC)",
            "自适应 MVS" | "自适应" => "Adaptive",
            "全色" => "Full Color",
            "跟随系统" => "System",
            "浅色" => "Light",
            "深色" => "Dark",
            "复制" => "Copy",
            "粘贴" => "Paste",
            "强制退出" => "Force Quit",
            "安全选项" => "Security options",
            "切换远程窗口" => "Switch remote window",
            "显示桌面" => "Show desktop",
            "全局" => "Global",
            "会话" => "Session",
            "取消" => "Cancel",
            "关闭" => "Close",
            "已复制设备地址" => "Device address copied",
            "已复制用户名" => "Username copied",
            "请输入有效的设备地址和端口" => "Enter a valid device address and port",
            "历史连接已更新" => "Connection history updated",
            "已加入历史连接" => "Added to connection history",
            "按键映射已恢复默认" => "Key mappings restored to defaults",
            "请输入设备地址和用户名" => "Enter a device address and username",
            "请输入有效端口（1–65535）" => "Enter a valid port (1-65535)",
            "请输入密码" => "Enter a password",
            "保存的密码不可用，请重新输入" => {
                "The saved password is unavailable. Enter it again."
            }
            "已复制按键预设" => "Key preset duplicated",
            "已添加常用映射" => "Mapping added",
            "已移除按键映射" => "Key mapping removed",
            "远程键鼠输入已启用" => "Remote keyboard and mouse input enabled",
            "远程键盘已启用，输入将发送到远端" => {
                "Remote keyboard enabled; input goes to the remote"
            }
            "已切换到实际画面（1:1）" => "Switched to actual size (1:1)",
            "已向远端发送截屏指令" => "Screenshot request sent to remote",
            "已切换到远端应用列表" => "Switched to remote app switcher",
            "已打开远端调度中心" => "Opened remote Mission Control",
            "已显示远端桌面" => "Showed remote desktop",
            "快捷按钮已隐藏" => "Toolbar button hidden",
            "快捷按钮已显示" => "Toolbar button shown",
            "窗口聚焦时系统快捷键将发送到远端" => {
                "System shortcuts will be sent to the remote while the window is focused"
            }
            "系统快捷键保留在本机" => "System shortcuts will stay local",
            "已删除历史连接" => "Connection history entry deleted",
            "正在当前 Session 窗口中连接…" => {
                "Connecting in the current session window..."
            }
            "无法确定导出目录" => "Could not determine the export directory",
            "退出 ARD Viewer？" => "Quit ARD Viewer?",
            "打开的设置和远程会话也会一并关闭。" => {
                "Open settings and remote sessions will also close."
            }
            "关闭设置窗口？" => "Close the settings window?",
            "确认关闭当前设置窗口。" => "Close the current settings window.",
            "关闭远程会话？" => "Close the remote session?",
            "当前远程连接将会断开。" => {
                "The current remote connection will be disconnected."
            }
            "会话缓存已损坏" => "The session buffer is corrupted",
            "无法将远程 framebuffer 转换为 RGBA 显示数据" => {
                "Could not convert the remote framebuffer to RGBA display data"
            }
            "远程输入缓存已满" => "The remote input buffer is full",
            "远程输入调度器已停止" => "The remote input dispatcher has stopped",
            "远程输入尚未就绪" => "Remote input is not ready",
            _ => value,
        }
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_codes_are_stable() {
        assert_eq!(Language::from_code("en"), Language::English);
        assert_eq!(Language::English.code(), "en");
        assert_eq!(Language::from_code("unknown"), Language::SimplifiedChinese);
    }
}
