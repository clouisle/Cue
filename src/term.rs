//! ANSI 调色板与服务名前缀格式化。

/// 按服务序循环取色的 8 色调色板（明亮色系，适配深色终端）。
const PALETTE: [&str; 8] = ["36", "33", "35", "32", "34", "91", "96", "93"];

/// 服务序对应的 ANSI SGR 颜色码。
pub fn color_for(index: usize) -> &'static str {
    PALETTE[index % PALETTE.len()]
}

/// 用 ANSI SGR 码包裹文本；`code` 为空字符串时原样返回。
pub fn paint(code: &str, s: &str) -> String {
    if code.is_empty() {
        s.to_string()
    } else {
        format!("\x1b[{code}m{s}\x1b[0m")
    }
}

/// 组装 docker-compose 风格前缀：`{name填充到最宽}  | `。
pub fn prefix(name: &str, width: usize, color: &str) -> String {
    format!("{}  | ", paint(color, &format!("{name:<width$}")))
}

/// dim（灰色）输出，用于退出/重启等状态消息。
pub fn dim(s: &str) -> String {
    paint("2", s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_cycles_palette() {
        assert_eq!(color_for(0), "36");
        assert_eq!(color_for(8), "36");
        assert_eq!(color_for(9), "33");
    }

    #[test]
    fn paint_empty_code_passthrough() {
        assert_eq!(paint("", "x"), "x");
        assert_eq!(paint("31", "x"), "\x1b[31mx\x1b[0m");
    }

    #[test]
    fn prefix_pads_to_width() {
        assert_eq!(prefix("a", 4, ""), "a     | ");
        assert_eq!(prefix("abcd", 4, ""), "abcd  | ");
    }
}
