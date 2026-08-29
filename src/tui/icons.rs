//! Compact, terminal-safe Explorer icons inspired by VS Code file themes.
//!
//! The navigation pane deliberately uses emoji only: it works without a Nerd
//! Font and keeps icon selection independent from Perforce state badges.

pub fn explorer_icon(name: &str, is_directory: bool, expanded: bool) -> &'static str {
    if is_directory {
        return if expanded { "📂" } else { "📁" };
    }

    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "cargo.toml" | "package.json" | "pyproject.toml" | "go.mod" | "gemfile" => "📦",
        "cargo.lock" | "package-lock.json" | "yarn.lock" | "pnpm-lock.yaml" => "🔒",
        ".gitignore" | ".gitattributes" | ".p4ignore" | "p4ignore.txt" => "🙈",
        "makefile" | "justfile" | "cmakelists.txt" => "🔨",
        _ if lower.starts_with("readme") => "📖",
        _ if lower.starts_with("license") || lower == "copying" => "📜",
        _ if lower.starts_with("dockerfile") || lower.starts_with("docker-compose") => "🐳",
        _ if lower == ".env" || lower.starts_with(".env.") => "🔑",
        _ => match lower.rsplit_once('.').map(|(_, extension)| extension) {
            Some("rs") => "🦀",
            Some("py" | "pyi") => "🐍",
            Some("js" | "mjs" | "cjs") => "🟨",
            Some("ts") => "🔷",
            Some("jsx" | "tsx") => "🟦",
            Some("json" | "jsonc") => "🧾",
            Some("md" | "markdown") => "📝",
            Some("html" | "htm") => "🌐",
            Some("css" | "scss" | "sass" | "less") => "🎨",
            Some("toml" | "yaml" | "yml" | "ini" | "cfg" | "conf") => "🔧",
            Some("sh" | "bash" | "zsh" | "fish") => "🐚",
            Some("ps1" | "psm1" | "psd1" | "bat" | "cmd") => "💻",
            Some("c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh") => "🔩",
            Some("cs") => "🟣",
            Some("go") => "🐹",
            Some("rb") => "💎",
            Some("java" | "jar") => "☕",
            Some("lua") => "🌙",
            Some("sql" | "db" | "sqlite" | "sqlite3") => "💾",
            Some("csv" | "tsv") => "📊",
            Some("log") => "📋",
            Some("pdf") => "📕",
            Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "svg") => "📷",
            Some("mp3" | "wav" | "flac" | "ogg") => "🎵",
            Some("mp4" | "mkv" | "avi" | "mov" | "webm") => "🎬",
            Some("zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar") => "🧳",
            Some("exe" | "dll" | "so" | "dylib" | "a" | "o" | "bin" | "wasm") => "⚡",
            _ => "📄",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directories_and_common_file_types_are_distinct() {
        assert_eq!(explorer_icon("src", true, false), "📁");
        assert_eq!(explorer_icon("src", true, true), "📂");
        assert_eq!(explorer_icon("main.rs", false, false), "🦀");
        assert_eq!(explorer_icon("README.md", false, false), "📖");
        assert_eq!(explorer_icon("unknown.asset", false, false), "📄");
    }
}
