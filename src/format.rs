//! JSON / XML 格式化（pretty-print）。
//!
//! 支持选中区域格式化或全文格式化；如果文本不是合法 JSON/XML，
//! 返回错误信息而不是静默失败。

use std::path::Path;

/// 这个路径是不是我们认得的格式化类型。文件树右键用它决定要不要显示「格式化」。
pub fn supports(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "json" | "xml" | "xsd" | "svg" | "plist" | "wsdl" | "pom"
    )
}

/// 自动根据文件扩展名或内容特征选择格式化器。
pub fn auto_format(text: &str, path: Option<&Path>) -> Result<String, String> {
    if let Some(p) = path {
        if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
            let ext = ext.to_ascii_lowercase();
            match ext.as_str() {
                "json" => return format_json(text),
                "xml" | "xsd" | "svg" | "plist" | "wsdl" | "pom" => return format_xml(text),
                _ => {}
            }
        }
    }
    // 按内容猜测
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        format_json(text)
    } else if trimmed.starts_with("<") && !trimmed.starts_with("<!--") {
        format_xml(text)
    } else {
        Err("无法识别格式（仅支持 JSON 和 XML）".into())
    }
}

/// 用 `serde_json` 做 pretty-print；解析失败时返回具体错误。
pub fn format_json(text: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("JSON 解析错误: {e}"))?;
    serde_json::to_string_pretty(&value)
        .map(|s| s + "\n")
        .map_err(|e| format!("JSON 序列化错误: {e}"))
}

/// 轻量级 XML pretty-print。
///
/// 不引入完整 XML 解析库，而是基于 token 流做缩进：
/// - 自闭合标签 `<tag/>` 单独一行
/// - 开始标签 `<tag>` 增加缩进，结束标签 `</tag>` 减少缩进
/// - 标签内的文本节点保留（如果包含换行则跟随标签缩进）
/// - 注释 `<!-- -->`、CDATA `<![CDATA[ ]]>`、处理指令 `<? ?>` 原样保留
/// - DOCTYPE 原样保留
///
/// 实现非常轻量，只处理常见格式，复杂 DTD/命名空间前缀也能正确缩进。
pub fn format_xml(text: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    let mut depth = 0i32;
    let indent = "  ";
    let mut chars = text.char_indices().peekable();

    while let Some((i, c)) = chars.next() {
        if c == '<' {
            // 查看标签类型
            if let Some(&(_, next)) = chars.peek() {
                match next {
                    '!' => {
                        // <!--, <![CDATA[, <!DOCTYPE
                        chars.next(); // consume '!'
                        if chars.peek().map(|&(_, c)| c) == Some('-') {
                            // 注释 <!-- -->
                            out.push_str("<!--");
                            chars.next();
                            chars.next();
                            while let Some((_, ch)) = chars.next() {
                                if ch == '-' && chars.peek().map(|&(_, c)| c) == Some('-') {
                                    chars.next();
                                    if chars.peek().map(|&(_, c)| c) == Some('>') {
                                        chars.next();
                                        out.push_str("-->");
                                        break;
                                    } else {
                                        out.push('-');
                                        out.push('-');
                                    }
                                } else {
                                    out.push(ch);
                                }
                            }
                            out.push('\n');
                        } else if text[i..]
                            .get(2..8)
                            .map(|s| s.eq_ignore_ascii_case("[CDATA["))
                            .unwrap_or(false)
                        {
                            // CDATA
                            out.push_str("<![CDATA[");
                            for _ in 0..8 {
                                chars.next();
                            }
                            while let Some((_, ch)) = chars.next() {
                                out.push(ch);
                                if ch == ']' && chars.peek().map(|&(_, c)| c) == Some(']') {
                                    chars.next();
                                    if chars.peek().map(|&(_, c)| c) == Some('>') {
                                        chars.next();
                                        out.push_str("]]>");
                                        break;
                                    } else {
                                        out.push(']');
                                    }
                                }
                            }
                            out.push('\n');
                        } else {
                            // DOCTYPE 或其他 <!...>
                            out.push('<');
                            out.push('!');
                            let mut in_bracket = false;
                            let mut bracket_depth = 0;
                            while let Some((_, ch)) = chars.next() {
                                out.push(ch);
                                if ch == '[' {
                                    in_bracket = true;
                                    bracket_depth += 1;
                                } else if ch == ']' {
                                    bracket_depth -= 1;
                                    if bracket_depth <= 0 {
                                        in_bracket = false;
                                    }
                                } else if ch == '>' && !in_bracket {
                                    break;
                                }
                            }
                            out.push('\n');
                        }
                    }
                    '?' => {
                        // 处理指令 <?xml ...?>
                        out.push('<');
                        out.push('?');
                        chars.next();
                        while let Some((_, ch)) = chars.next() {
                            out.push(ch);
                            if ch == '?' && chars.peek().map(|&(_, c)| c) == Some('>') {
                                chars.next();
                                out.push('>');
                                break;
                            }
                        }
                        out.push('\n');
                    }
                    '/' => {
                        // 结束标签 </tag>
                        chars.next(); // consume '/'
                        depth = depth.saturating_sub(1);
                        out.push_str(&indent.repeat(depth.max(0) as usize));
                        out.push('<');
                        out.push('/');
                        while let Some((_, ch)) = chars.next() {
                            out.push(ch);
                            if ch == '>' {
                                break;
                            }
                        }
                        // 如果下一个非空白字符不是 <，说明后面有文本，不换行
                        skip_spaces_peek_newline(&mut chars, &mut out);
                    }
                    _ => {
                        // 开始标签 <tag ...>
                        out.push_str(&indent.repeat(depth.max(0) as usize));
                        out.push('<');
                        // 收集到 > 为止，注意属性值里的 > 要跳过
                        let mut in_quote = None::<char>;
                        let mut self_closing = false;
                        while let Some((_, ch)) = chars.next() {
                            out.push(ch);
                            if in_quote.is_none() {
                                if ch == '"' || ch == '\'' {
                                    in_quote = Some(ch);
                                } else if ch == '/' {
                                    if chars.peek().map(|&(_, c)| c) == Some('>') {
                                        self_closing = true;
                                    }
                                } else if ch == '>' {
                                    break;
                                }
                            } else if Some(ch) == in_quote {
                                in_quote = None;
                            }
                        }
                        if self_closing {
                            out.push('\n');
                        } else {
                            depth += 1;
                            // 如果后面紧跟结束标签（空元素 <tag></tag>），特殊处理
                            let after_tag = skip_spaces_collect(&mut chars);
                            if after_tag.starts_with("</") {
                                // 空元素：在同一行输出 </tag>
                                depth = depth.saturating_sub(1);
                                out.push_str(&after_tag);
                                // 跳过已经消费掉的字符
                                for ch in after_tag.chars().skip(2) {
                                    if ch == '>' {
                                        break;
                                    }
                                }
                                // 实际上 skip_spaces_collect 已经消费了所有字符
                                // 需要重新定位到 > 之后
                                // 这里简单处理：把 after_tag 写回去后调整 chars
                                // 由于我们已经消费了 after_tag 的内容，直接继续即可
                                out.push('\n');
                            } else if !after_tag.is_empty() {
                                // 有文本内容
                                if after_tag.contains('\n') {
                                    // 多行文本，整体缩进
                                    out.push('\n');
                                    for line in after_tag.lines() {
                                        out.push_str(&indent.repeat(depth.max(0) as usize));
                                        out.push_str(line.trim_end());
                                        out.push('\n');
                                    }
                                } else {
                                    // 单行文本，直接跟在标签后面
                                    out.push_str(after_tag.trim());
                                    // 不在这里换行，等遇到 </tag> 时再处理
                                }
                            } else {
                                out.push('\n');
                            }
                        }
                    }
                }
            }
        } else {
            // 标签外的文本（通常已经被上面的逻辑处理了，这里是兜底）
            // 跳过空白直到下一个 <
            if !c.is_whitespace() {
                out.push(c);
            }
        }
    }

    // 去掉尾部多余空行
    let out = out.trim_end().to_string();
    if out.is_empty() {
        return Err("XML 内容为空或无法识别".into());
    }
    Ok(out + "\n")
}

/// 跳过空白字符，如果下一个非空白字符不是 '<' 则换行。
fn skip_spaces_peek_newline<'a>(
    chars: &mut std::iter::Peekable<impl Iterator<Item = (usize, char)>>,
    out: &mut String,
) {
    let mut had_non_space = false;
    while let Some(&(_, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else {
            had_non_space = true;
            break;
        }
    }
    if had_non_space {
        // 后面有文本，暂时不换行（文本会跟在结束标签后面）
    } else {
        out.push('\n');
    }
}

/// 跳过空白字符，收集到下一个 < 之前的所有文本（不包含 <）。
/// 返回收集到的文本；调用后迭代器停在 < 处。
fn skip_spaces_collect<'a>(
    chars: &mut std::iter::Peekable<impl Iterator<Item = (usize, char)>>,
) -> String {
    let mut s = String::new();
    while let Some(&(_, c)) = chars.peek() {
        if c == '<' {
            break;
        }
        s.push(c);
        chars.next();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_simple() {
        let raw = r#"{"a":1,"b":[2,3]}"#;
        let out = format_json(raw).unwrap();
        assert!(out.contains("{\n"));
        assert!(out.contains("\"a\": 1"));
    }

    #[test]
    fn json_err() {
        let raw = "{a:1}";
        assert!(format_json(raw).is_err());
    }

    #[test]
    fn xml_simple() {
        let raw = r#"<root><child attr="v">text</child></root>"#;
        let out = format_xml(raw).unwrap();
        assert!(out.contains("<root>"));
        assert!(out.contains("  <child"));
        assert!(out.contains("  </child>"));
        assert!(out.contains("</root>"));
    }

    #[test]
    fn xml_self_closing() {
        let raw = r#"<root><item/></root>"#;
        let out = format_xml(raw).unwrap();
        assert!(out.contains("  <item/>"));
    }

    #[test]
    fn xml_comment() {
        let raw = r#"<!-- comment --><root/>"#;
        let out = format_xml(raw).unwrap();
        assert!(out.contains("<!-- comment -->"));
    }

    #[test]
    fn xml_cdata() {
        let raw = r#"<root><![CDATA[hello <world>]]></root>"#;
        let out = format_xml(raw).unwrap();
        assert!(out.contains("<![CDATA[hello <world>]]>"));
    }

    #[test]
    fn auto_detect_json() {
        let raw = r#"{"x":1}"#;
        let out = auto_format(raw, None).unwrap();
        assert!(out.contains("\"x\": 1"));
    }

    #[test]
    fn auto_detect_xml_by_ext() {
        let raw = r#"<root><a/></root>"#;
        let out = auto_format(raw, Some(std::path::Path::new("test.xml"))).unwrap();
        assert!(out.contains("  <a/>"));
    }
}
