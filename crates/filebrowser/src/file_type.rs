use std::path::Path;

pub fn is_text_file(path: &str) -> bool {
    matches!(
        Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase()
            .as_str(),
        "txt"
            | "md"
            | "rst"
            | "html"
            | "htm"
            | "css"
            | "js"
            | "ts"
            | "jsx"
            | "tsx"
            | "json"
            | "jsonc"
            | "yml"
            | "yaml"
            | "toml"
            | "ini"
            | "cfg"
            | "conf"
            | "rs"
            | "py"
            | "rb"
            | "go"
            | "java"
            | "c"
            | "cpp"
            | "cc"
            | "h"
            | "hpp"
            | "cs"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "ps1"
            | "bat"
            | "cmd"
            | "xml"
            | "svg"
            | "csv"
            | "sql"
            | "graphql"
            | "gql"
            | "dockerfile"
            | "makefile"
            | "gitignore"
            | "env"
            | "lock"
            | "proto"
            | "tf"
            | "hcl"
            | "nix"
            | "r"
            | "lua"
            | "perl"
            | "pl"
            | "php"
            | "swift"
            | "kt"
            | "kts"
            | "scala"
            | "ex"
            | "exs"
            | "clj"
            | "cljs"
            | "erl"
            | "hrl"
            | "hs"
            | "lhs"
            | "ml"
            | "mli"
            | "vue"
            | "svelte"
            | "astro"
    )
}

pub fn sniff_is_text(path: &Path) -> bool {
    use std::io::Read;
    const SAMPLE: usize = 8 * 1024;

    let mut buf = vec![0u8; SAMPLE];
    let n = match std::fs::File::open(path) {
        Err(_) => return false,
        Ok(mut f) => match f.read(&mut buf) {
            Err(_) => return false,
            Ok(n) => n,
        },
    };

    if n == 0 {
        return true;
    }

    let sample = &buf[..n];

    if sample.iter().any(|&b| b == 0x00) {
        return false;
    }

    let suspicious = sample
        .iter()
        .filter(|&&b| b < 0x09 || (b > 0x0d && b < 0x20 && b != 0x1b))
        .count();

    (suspicious as f64 / n as f64) < 0.10
}

pub fn monaco_language(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "rs" => "rust",
        "py" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" | "mts" => "typescript",
        "jsx" => "javascript",
        "tsx" => "typescript",
        "json" | "jsonc" => "json",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" | "sass" => "scss",
        "md" | "markdown" => "markdown",
        "yml" | "yaml" => "yaml",
        "toml" => "toml",
        "xml" | "svg" => "xml",
        "sh" | "bash" | "zsh" | "fish" => "shell",
        "ps1" => "powershell",
        "bat" | "cmd" => "bat",
        "sql" => "sql",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "cc" | "hpp" | "cxx" => "cpp",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "kt" | "kts" => "kotlin",
        "scala" => "scala",
        "r" => "r",
        "lua" => "lua",
        "tf" | "hcl" => "hcl",
        "proto" => "proto",
        "graphql" | "gql" => "graphql",
        "vue" => "html",
        "svelte" => "html",
        "csv" => "plaintext",
        "txt" | "rst" | "ini" | "cfg" | "conf" | "env" => "plaintext",
        _ => "plaintext",
    }
}
