use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Makes text from another program safe to draw into one row of cells:
/// tabs become spaces up to the next 4 column stop, and line breaks and other
/// control characters are dropped, since a terminal would act on them and
/// move the cursor out of the cell grid.
pub(crate) fn sanitize_line(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut width = 0usize;
    for ch in text.chars() {
        if ch == '\t' {
            let pad = 4 - width % 4;
            output.extend(std::iter::repeat_n(' ', pad));
            width += pad;
        } else if !ch.is_control() {
            output.push(ch);
            width += UnicodeWidthChar::width(ch).unwrap_or(0);
        }
    }
    output
}

pub(crate) fn truncate_end(text: &str, max_width: usize) -> String {
    if display_width(text) <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }

    let prefix = take_prefix_width(text, max_width.saturating_sub(1));
    format!("{prefix}…")
}

fn take_prefix_width(text: &str, max_width: usize) -> String {
    let mut output = String::new();
    let mut width = 0usize;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > max_width {
            break;
        }
        output.push(ch);
        width += ch_width;
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_end_uses_display_width() {
        let text = truncate_end("提交 herdr 的反馈", 16);

        assert_eq!(text, "提交 herdr 的反…");
        assert!(display_width(&text) <= 16);
    }
}
