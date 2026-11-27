use std::sync::LazyLock;

use fancy_regex::Regex;

use super::re;

pub(super) static NEWLINE: LazyLock<Regex> = LazyLock::new(|| re(r"^(?:[ \t]*(?:\n|$))+"));
pub(super) static CODE: LazyLock<Regex> =
    LazyLock::new(|| re(r"^((?: {4}| {0,3}\t)[^\n]+(?:\n(?:[ \t]*(?:\n|$))*)?)+"));
pub(super) static FENCES: LazyLock<Regex> = LazyLock::new(|| {
    re(r"^ {0,3}(`{3,}(?=[^`\n]*(?:\n|$))|~{3,})([^\n]*)(?:\n|$)(?:|([\s\S]*?)(?:\n|$))(?: {0,3}\1[~`]* *(?=\n|$)|$)")
});
pub(super) static HR: LazyLock<Regex> =
    LazyLock::new(|| re(r"^ {0,3}((?:-[\t ]*){3,}|(?:_[ \t]*){3,}|(?:\*[ \t]*){3,})(?:\n+|$)"));
pub(super) static HEADING: LazyLock<Regex> = LazyLock::new(|| re(r"^ {0,3}(#{1,6})(?=\s|$)(.*)(?:\n+|$)"));
pub(super) static BLOCKQUOTE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^( {0,3}> ?(([^\n]+(?:\n(?! {0,3}((?:-[\t ]*){3,}|(?:_[ \t]*){3,}|(?:\*[ \t]*){3,})(?:\n+|$)| {0,3}#{1,6}(?:\s|$)| {0,3}>| {0,3}(?:`{3,}(?=[^`\n]*(?:\n|$))|~~~)[^\n]*(?:\n|$)| {0,3}(?:[*+-]|\d{1,9}[.)])(?:[ \t]|\n|$)|</?(?:address|article|aside|base|basefont|blockquote|body|caption|center|col|colgroup|dd|details|dialog|dir|div|dl|dt|fieldset|figcaption|figure|footer|form|frame|frameset|h[1-6]|head|header|hr|html|iframe|legend|li|link|main|menu|menuitem|meta|nav|noframes|ol|optgroup|option|p|param|search|section|summary|table|tbody|td|tfoot|th|thead|title|tr|track|ul)(?: +|\n|/?>)|<(?:script|pre|style|textarea|!--)|[ \t]+\n)[^\n]+)*)|[^\n]*)(?:\n|$))+",
    )
});
pub(super) static LIST: LazyLock<Regex> =
    LazyLock::new(|| re(r"^( {0,3}(?:[*+-]|\d{1,9}[.)]))([ \t][^\n]*?)?(?:\n|$)"));
pub(super) static HTML: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r#"(?i)^ {0,3}(?:<(script|pre|style|textarea)[\s>][\s\S]*?(?:</\1>[^\n]*\n*|$)|<!--(?:-?>|[\s\S]*?(?:-->|$))[^\n]*(\n+|$)|<\?[\s\S]*?(?:\?>[^\n]*\n*|$)|<![A-Z][\s\S]*?(?:>[^\n]*\n*|$)|<!\[CDATA\[[\s\S]*?(?:\]\]>[^\n]*\n*|$)|</?(address|article|aside|base|basefont|blockquote|body|caption|center|col|colgroup|dd|details|dialog|dir|div|dl|dt|fieldset|figcaption|figure|footer|form|frame|frameset|h[1-6]|head|header|hr|html|iframe|legend|li|link|main|menu|menuitem|meta|nav|noframes|ol|optgroup|option|p|param|search|section|summary|table|tbody|td|tfoot|th|thead|title|tr|track|ul)(?: +|\n|/?>)[\s\S]*?(?:(?:\n[ 	]*)+\n|$)|<(?!script|pre|style|textarea)([a-z][A-Za-z0-9_-]*)(?: +[a-zA-Z:_][A-Za-z0-9_.:-]*(?: *= *"[^"\n]*"| *= *'[^'\n]*'| *= *[^\s"'=<>`]+)?)*? */?>(?=[ \t]*(?:\n|$))[\s\S]*?(?:(?:\n[ 	]*)+\n|$)|</(?!script|pre|style|textarea)[a-z][A-Za-z0-9_-]*\s*>(?=[ \t]*(?:\n|$))[\s\S]*?(?:(?:\n[ 	]*)+\n|$))"#,
    )
});
pub(super) static DEF: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r#"^ {0,3}\[((?!\s*\])(?:\\[\s\S]|[^\[\]\\])+)\]: *(?:\n[ \t]*)?([^<\s][^\s]*|<.*?>)(?:(?: +(?:\n[ \t]*)?| *\n[ \t]*)((?:"(?:\\"?|[^"\\])*"|'[^'\n]*(?:\n[^'\n]+)*\n?'|\([^()]*\))))? *(?:\n+|$)"#,
    )
});
pub(super) static TABLE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^ *([^\n ].*)\n {0,3}((?:\| *)?:?-+:? *(?:\| *:?-+:? *)*(?:\| *)?)(?:\n((?:(?! *\n| {0,3}((?:-[\t ]*){3,}|(?:_[ \t]*){3,}|(?:\*[ \t]*){3,})(?:\n+|$)| {0,3}#{1,6}(?:\s|$)| {0,3}>|(?: {4}| {0,3}	)[^\n]| {0,3}(?:`{3,}(?=[^`\n]*(?:\n|$))|~~~)[^\n]*(?:\n|$)| {0,3}(?:[*+-]|1[.)])[ \t]|</?(?:address|article|aside|base|basefont|blockquote|body|caption|center|col|colgroup|dd|details|dialog|dir|div|dl|dt|fieldset|figcaption|figure|footer|form|frame|frameset|h[1-6]|head|header|hr|html|iframe|legend|li|link|main|menu|menuitem|meta|nav|noframes|ol|optgroup|option|p|param|search|section|summary|table|tbody|td|tfoot|th|thead|title|tr|track|ul)(?: +|\n|/?>)|<(?:script|pre|style|textarea|!--)).*(?:\n|$))*)\n*|$)",
    )
});
pub(super) static LHEADING: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^(?! {0,3}(?:[*+-]|\d{1,9}[.)]) |(?: {4}| {0,3}\t)| {0,3}(?:`{3,}|~{3,})| {0,3}>| {0,3}#{1,6}(?:\s|$)| {0,3}<[^\n>]+>\n| {0,3}\|?(?:[:\- ]*\|)+[\:\- ]*\n)((?:.|\n(?!\s*?\n| {0,3}(?:[*+-]|\d{1,9}[.)]) |(?: {4}| {0,3}\t)| {0,3}(?:`{3,}|~{3,})| {0,3}>| {0,3}#{1,6}(?:\s|$)| {0,3}<[^\n>]+>\n| {0,3}\|?(?:[:\- ]*\|)+[\:\- ]*\n))+?)\n {0,3}(=+|-+) *(?:\n+|$)",
    )
});
pub(super) static PARAGRAPH: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^([^\n]+(?:\n(?! {0,3}((?:-[\t ]*){3,}|(?:_[ \t]*){3,}|(?:\*[ \t]*){3,})(?:\n+|$)| {0,3}#{1,6}(?:\s|$)| {0,3}>| {0,3}(?:`{3,}(?=[^`\n]*(?:\n|$))|~~~)[^\n]*(?:\n|$)| {0,3}(?:[*+-]|1[.)])[ \t]+[^ \t\n]|</?(?:address|article|aside|base|basefont|blockquote|body|caption|center|col|colgroup|dd|details|dialog|dir|div|dl|dt|fieldset|figcaption|figure|footer|form|frame|frameset|h[1-6]|head|header|hr|html|iframe|legend|li|link|main|menu|menuitem|meta|nav|noframes|ol|optgroup|option|p|param|search|section|summary|table|tbody|td|tfoot|th|thead|title|tr|track|ul)(?: +|\n|/?>)|<(?:script|pre|style|textarea|!--)| *([^\n ].*)\n {0,3}((?:\| *)?:?-+:? *(?:\| *:?-+:? *)*(?:\| *)?)(?:\n((?:(?! *\n| {0,3}((?:-[\t ]*){3,}|(?:_[ \t]*){3,}|(?:\*[ \t]*){3,})(?:\n+|$)| {0,3}#{1,6}(?:\s|$)| {0,3}>|(?: {4}| {0,3}	)[^\n]| {0,3}(?:`{3,}(?=[^`\n]*(?:\n|$))|~~~)[^\n]*(?:\n|$)| {0,3}(?:[*+-]|1[.)])[ \t]|</?(?:address|article|aside|base|basefont|blockquote|body|caption|center|col|colgroup|dd|details|dialog|dir|div|dl|dt|fieldset|figcaption|figure|footer|form|frame|frameset|h[1-6]|head|header|hr|html|iframe|legend|li|link|main|menu|menuitem|meta|nav|noframes|ol|optgroup|option|p|param|search|section|summary|table|tbody|td|tfoot|th|thead|title|tr|track|ul)(?: +|\n|/?>)|<(?:script|pre|style|textarea|!--)).*(?:\n|$))*)\n*|$)|[ \t]+\n)[^\n]+)*)",
    )
});
pub(super) static TEXT: LazyLock<Regex> = LazyLock::new(|| re(r"^[^\n]+"));
pub(super) static ESCAPE: LazyLock<Regex> = LazyLock::new(|| re(r##"^\\([!"#$%&'()*+,\-./:;<=>?@\[\]\\^_`{|}~])"##));
pub(super) static TAG: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r#"^<!--(?:-?>|[\s\S]*?-->)|^</[a-zA-Z][A-Za-z0-9_:-]*\s*>|^<[a-zA-Z][A-Za-z0-9_-]*(?:\s+[a-zA-Z:_][A-Za-z0-9_.:-]*(?:\s*=\s*"[^"]*"|\s*=\s*'[^']*'|\s*=\s*[^\s"'=<>`]+)?)*?\s*/?>|^<\?[\s\S]*?\?>|^<![a-zA-Z]+\s[\s\S]*?>|^<!\[CDATA\[[\s\S]*?\]\]>"#,
    )
});
pub(super) static LINK: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r#"^!?\[((?:\[(?:\\[\s\S]|[^\[\]\\])*\]|\\[\s\S]|`+(?!`)[^`]*?`+(?!`)|``+(?=\])|[^\[\]\\`])*?)\]\(\s*(<(?:\\.|[^\n<>\\])+>|[^ \t\n\x00-\x1f]+|(?=\)))(?:(?:[ \t]+(?:\n[ \t]*)?|\n[ \t]*)("(?:\\"?|[^"\\])*"|'(?:\\'?|[^'\\])*'|\((?:\\\)?|[^)\\])*\)))?\s*\)"#,
    )
});
pub(super) static REFLINK: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^!?\[((?:\[(?:\\[\s\S]|[^\[\]\\])*\]|\\[\s\S]|`+(?!`)[^`]*?`+(?!`)|``+(?=\])|[^\[\]\\`])*?)\]\[((?!\s*\])(?:\\[\s\S]|[^\[\]\\])+)\]",
    )
});
pub(super) static NOLINK: LazyLock<Regex> = LazyLock::new(|| re(r"^!?\[((?!\s*\])(?:\\[\s\S]|[^\[\]\\])+)\](?:\[\])?"));
pub(super) static REFLINK_SEARCH: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"!?\[((?:\[(?:\\[\s\S]|[^\[\]\\])*\]|\\[\s\S]|`+(?!`)[^`]*?`+(?!`)|``+(?=\])|[^\[\]\\`])*?)\]\[((?!\s*\])(?:\\[\s\S]|[^\[\]\\])+)\]|!?\[((?!\s*\])(?:\\[\s\S]|[^\[\]\\])+)\](?:\[\])?(?!\()",
    )
});
pub(super) static BLOCK_SKIP: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"\[(?:[^\[\]`]|(?<a>`+)[^`]+\k<a>(?!`))*?\]\((?:\\[\s\S]|[^\\\(\)]|\((?:\\[\s\S]|[^\\\(\)])*\))*\)|(?<!`)()(?<b>`+)[^`]+\k<b>(?!`)|<(?! )[^<>]*?>",
    )
});
pub(super) static ANY_PUNCT: LazyLock<Regex> = LazyLock::new(|| re(r"\\([\p{P}\p{S}])"));
pub(super) static EM_L: LazyLock<Regex> =
    LazyLock::new(|| re(r"^(?:\*+(?:((?!\*)(?!~)[\p{P}\p{S}])|([^\s*]))?)|^_+(?:((?!_)(?!~)[\p{P}\p{S}])|([^\s_]))?"));
pub(super) static EM_R_AST_SKIP: LazyLock<Regex> = LazyLock::new(|| re(r"^[^_*]*?__[^_*]*?\*[^_*]*?(?=__)"));
pub(super) static EM_R_UND_SKIP: LazyLock<Regex> = LazyLock::new(|| re(r"^[^_*]*?\*\*[^_*]*?_[^_*]*?(?=\*\*)"));
pub(super) static PUNCT_OR_SYMBOL: LazyLock<Regex> = LazyLock::new(|| re(r"[\p{P}\p{S}]"));
#[cfg(test)]
pub(super) static EM_R_AST: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^[^_*]*?__[^_*]*?\*[^_*]*?(?=__)|[^*]+(?=[^*])|(?!\*)(?!~)[\p{P}\p{S}](\*+)(?=[\s]|$)|(?:[^\s\p{P}\p{S}]|~)(\*+)(?!\*)(?=(?!~)[\s\p{P}\p{S}]|$)|(?!\*)(?!~)[\s\p{P}\p{S}](\*+)(?=(?:[^\s\p{P}\p{S}]|~))|[\s](\*+)(?!\*)(?=(?!~)[\p{P}\p{S}])|(?!\*)(?!~)[\p{P}\p{S}](\*+)(?!\*)(?=(?!~)[\p{P}\p{S}])|(?:[^\s\p{P}\p{S}]|~)(\*+)(?=(?:[^\s\p{P}\p{S}]|~))",
    )
});
#[cfg(test)]
pub(super) static EM_R_UND: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^[^_*]*?\*\*[^_*]*?_[^_*]*?(?=\*\*)|[^_]+(?=[^_])|(?!_)[\p{P}\p{S}](_+)(?=[\s]|$)|[^\s\p{P}\p{S}](_+)(?!_)(?=[\s\p{P}\p{S}]|$)|(?!_)[\s\p{P}\p{S}](_+)(?=[^\s\p{P}\p{S}])|[\s](_+)(?!_)(?=[\p{P}\p{S}])|(?!_)[\p{P}\p{S}](_+)(?!_)(?=[\p{P}\p{S}])",
    )
});
pub(super) static CODESPAN: LazyLock<Regex> = LazyLock::new(|| re(r"^(`+)([^`]|[^`][\s\S]*?[^`])\1(?!`)"));
pub(super) static BR: LazyLock<Regex> = LazyLock::new(|| re(r"^( {2,}|\\)\n(?!\s*$)"));
pub(super) static AUTOLINK: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^<([a-zA-Z][a-zA-Z0-9+.-]{1,31}:[^\s\x00-\x1f<>]*|[a-zA-Z0-9.!#$%&'*+/=?_`{|}~-]+(@)[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?(?:\.[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?)+(?![-_]))>",
    )
});
pub(super) static URL: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^((?:[hH][tT][tT][pP][sS]?|[fF][tT][pP])://|www\.)(?:[a-zA-Z0-9\-]+\.?)+[^\s<]*|^[A-Za-z0-9._+-]+(@)[a-zA-Z0-9-_]+(?:\.[a-zA-Z0-9-_]*[a-zA-Z0-9])+(?![-_])",
    )
});
pub(super) static BACKPEDAL: LazyLock<Regex> =
    LazyLock::new(|| re(r#"(?:[^?!.,:;*_'"~()&]+|\([^)]*\)|&(?![a-zA-Z0-9]+;$)|[?!.,:;*_'"~)]+(?!$))+"#));
#[cfg(test)]
pub(super) static INLINE_TEXT: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^(`+|~+|[^`~])(?:(?=[`~])|(?= {2,}\n)|(?=[a-zA-Z0-9.!#$%&'*+/=?_`{\|}~-]+@)|[\s\S]*?(?:(?=[\\<!\[`*~_]|(?<![A-Za-z0-9_])_|[hH][tT][tT][pP][sS]?|[fF][tT][pP]://|www\.|$)|[^ ](?= {2,}\n)|[^a-zA-Z0-9.!#$%&'*+/=?_`{\|}~-](?=[a-zA-Z0-9.!#$%&'*+/=?_`{\|}~-]+@)))",
    )
});
pub(super) static PUNCT: LazyLock<Regex> = LazyLock::new(|| re(r"^((?![*_])[\s\p{P}\p{S}])"));
pub(super) static BULLET_DEPTH: LazyLock<Regex> = LazyLock::new(|| re(r"^\[[ xX]\] +\S"));
pub(super) static TASK: LazyLock<Regex> = LazyLock::new(|| re(r"^\[[ xX]\] +"));
pub(super) static CHECKBOX: LazyLock<Regex> = LazyLock::new(|| re(r"\[[ xX]\]"));
pub(super) static BQ_START: LazyLock<Regex> = LazyLock::new(|| re(r"^ {0,3}>"));
pub(super) static BQ_SETEXT: LazyLock<Regex> = LazyLock::new(|| re(r"\n {0,3}((?:=+|-+) *)(?=\n|$)"));
pub(super) static BQ_PREFIX: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^ {0,3}>[ \t]?"));
pub(super) static DOUBLE_BLANK: LazyLock<Regex> = LazyLock::new(|| re(r"\n[ \t]*\n[ \t]*$"));
pub(super) static ANY_LINE: LazyLock<Regex> = LazyLock::new(|| re(r"\n.*\n"));
pub(super) static INDENT_COMP: LazyLock<Regex> = LazyLock::new(|| re(r"^(\s+)(?:```)"));
pub(super) static CODE_INDENT: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^(?: {1,4}| {0,3}\t)"));
pub(super) static STRICT_DEL: LazyLock<Regex> =
    LazyLock::new(|| re(r"^(~~)(?=[^\s~])((?:\\.|[^\\])*?(?:\\.|[^\s~\\]))\1(?=[^~]|$)"));
/// `(?:^|\n) {0,3}(?:\$\$|\\\[)` split into its two branches, so the `\n` one can be searched once per document.
pub(super) static LATEX_BLOCK_AT_START: LazyLock<Regex> = LazyLock::new(|| re(r"^ {0,3}(?:\$\$|\\\[)"));
pub(super) static LATEX_BLOCK_AFTER_NEWLINE: LazyLock<Regex> = LazyLock::new(|| re(r"\n {0,3}(?:\$\$|\\\[)"));
pub(super) static LATEX_DOLLAR: LazyLock<Regex> =
    LazyLock::new(|| re(r"^ {0,3}\$\$[ \t]*(?:\n)?([\s\S]*?)\$\$[ \t]*(?:\n|$)"));
pub(super) static LATEX_BRACKET: LazyLock<Regex> =
    LazyLock::new(|| re(r"^ {0,3}\\\[[ \t]*(?:\n)?([\s\S]*?)\\\][ \t]*(?:\n|$)"));
pub(super) static LATEX_PENDING_BRACKET: LazyLock<Regex> = LazyLock::new(|| re(r"^ {0,3}\\\[[ \t]*(?:\n)?([\s\S]*)$"));
pub(super) static LATEX_PENDING_DOLLAR: LazyLock<Regex> = LazyLock::new(|| re(r"^ {0,3}\$\$[ \t]*(?:\n)?([\s\S]*)$"));
pub(super) static PENDING_MATH: LazyLock<Regex> = LazyLock::new(|| re(r"\\[A-Za-z]+|[_^=+*/<>()\[\]|±≤≥≠≈∈→⇒∞∫∑√-]"));
pub(super) static ENV_LIKE: LazyLock<Regex> = LazyLock::new(|| re(r"^[A-Z_][A-Z0-9_]*(?:[^A-Za-z0-9_\s])?$"));
pub(super) static IDENT_START: LazyLock<Regex> = LazyLock::new(|| re(r"^[A-Za-z_][A-Za-z0-9_]*"));
pub(super) static ALNUM: LazyLock<Regex> = LazyLock::new(|| re(r"[\p{L}\p{N}]"));

/// A list's item rule per bullet (`*`, `+`, `-`, `1.`, `1)`), compiled once instead of per list.
pub(super) static LIST_ITEM: LazyLock<[Regex; 5]> = LazyLock::new(|| {
    [r"\*", r"\+", r"\-", r"\d{1,9}\.", r"\d{1,9}\)"].map(|b| re(&format!(r"^( {{0,3}}{b})((?:[\t ][^\n]*)?(?:\n|$))")))
});

/// The lines that end a list item, for an item indent `lim + 1` (`lim` 0..=3).
pub(super) struct ItemEnds {
    pub next_bullet: Regex,
    pub hr: Regex,
    pub fences: Regex,
    pub heading: Regex,
    pub html: Regex,
    pub quote: Regex,
}

/// [`ItemEnds`] per `lim`, compiled once instead of per list item.
pub(super) static LIST_ITEM_ENDS: LazyLock<[ItemEnds; 4]> = LazyLock::new(|| {
    [0, 1, 2, 3].map(|lim| ItemEnds {
        next_bullet: re(&format!(
            r"^ {{0,{lim}}}(?:[*+-]|\d{{1,9}}[.)])((?:[ \t][^\n]*)?(?:\n|$))"
        )),
        hr: re(&format!(
            r"^ {{0,{lim}}}((?:- *){{3,}}|(?:_ *){{3,}}|(?:\* *){{3,}})(?:\n+|$)"
        )),
        fences: re(&format!(r"^ {{0,{lim}}}(?:```|~~~)")),
        heading: re(&format!(r"^ {{0,{lim}}}#")),
        html: re(&format!(r"(?i)^ {{0,{lim}}}<(?:[a-z].*>|!--)")),
        quote: re(&format!(r"^ {{0,{lim}}}>")),
    })
});
