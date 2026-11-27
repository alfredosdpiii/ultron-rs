const SYMBOLS: &[(&str, &str)] = &[
    ("Delta", "Δ"),
    ("Gamma", "Γ"),
    ("Im", "ℑ"),
    ("Join", "⋈"),
    ("Lambda", "Λ"),
    ("Leftarrow", "⇐"),
    ("Leftrightarrow", "⇔"),
    ("Longleftarrow", "⇐"),
    ("Longleftrightarrow", "⇔"),
    ("Longrightarrow", "⇒"),
    ("Omega", "Ω"),
    ("Phi", "Φ"),
    ("Pi", "Π"),
    ("Psi", "Ψ"),
    ("Re", "ℜ"),
    ("Rightarrow", "⇒"),
    ("Sigma", "Σ"),
    ("Theta", "Θ"),
    ("Upsilon", "Υ"),
    ("Vdash", "⊩"),
    ("Vert", "‖"),
    ("Vvdash", "⊪"),
    ("Xi", "Ξ"),
    ("aleph", "ℵ"),
    ("alpha", "α"),
    ("amalg", "⨿"),
    ("angle", "∠"),
    ("approx", "≈"),
    ("ast", "∗"),
    ("asymp", "≍"),
    ("backslash", "\\"),
    ("because", "∵"),
    ("beta", "β"),
    ("beth", "ℶ"),
    ("bigcap", "⋂"),
    ("bigcirc", "○"),
    ("bigcup", "⋃"),
    ("bigodot", "⨀"),
    ("bigoplus", "⨁"),
    ("bigotimes", "⨂"),
    ("bigsqcup", "⨆"),
    ("biguplus", "⨄"),
    ("bigvee", "⋁"),
    ("bigwedge", "⋀"),
    ("bot", "⊥"),
    ("bowtie", "⋈"),
    ("bullet", "•"),
    ("cap", "∩"),
    ("cdot", "·"),
    ("cdots", "⋯"),
    ("checkmark", "✓"),
    ("chi", "χ"),
    ("circ", "∘"),
    ("colon", ":"),
    ("complement", "∁"),
    ("cong", "≅"),
    ("coprod", "∐"),
    ("cup", "∪"),
    ("dagger", "†"),
    ("daleth", "ℸ"),
    ("dashv", "⊣"),
    ("ddagger", "‡"),
    ("ddots", "⋱"),
    ("delta", "δ"),
    ("div", "÷"),
    ("doteq", "≐"),
    ("dots", "…"),
    ("downarrow", "↓"),
    ("ell", "ℓ"),
    ("emptyset", "∅"),
    ("epsilon", "ϵ"),
    ("equiv", "≡"),
    ("eta", "η"),
    ("exists", "∃"),
    ("forall", "∀"),
    ("fullouterjoin", "⟗"),
    ("gamma", "γ"),
    ("ge", "≥"),
    ("geq", "≥"),
    ("geqslant", "≥"),
    ("gets", "←"),
    ("gg", "≫"),
    ("gimel", "ℷ"),
    ("hbar", "ℏ"),
    ("hookleftarrow", "↩"),
    ("hookrightarrow", "↪"),
    ("iff", "⇔"),
    ("iiint", "∭"),
    ("iint", "∬"),
    ("implies", "⇒"),
    ("in", "∈"),
    ("infty", "∞"),
    ("int", "∫"),
    ("iota", "ι"),
    ("kappa", "κ"),
    ("lVert", "‖"),
    ("lambda", "λ"),
    ("land", "∧"),
    ("langle", "⟨"),
    ("lbrace", "{"),
    ("lceil", "⌈"),
    ("ldots", "…"),
    ("le", "≤"),
    ("leadsto", "⇝"),
    ("leftarrow", "←"),
    ("leftharpoondown", "↽"),
    ("leftharpoonup", "↼"),
    ("leftouterjoin", "⟕"),
    ("leftrightarrow", "↔"),
    ("leftrightharpoons", "⇋"),
    ("leq", "≤"),
    ("leqslant", "≤"),
    ("lfloor", "⌊"),
    ("ll", "≪"),
    ("longleftarrow", "←"),
    ("longleftrightarrow", "↔"),
    ("longmapsto", "↦"),
    ("longrightarrow", "→"),
    ("lor", "∨"),
    ("lozenge", "◊"),
    ("ltimes", "⋉"),
    ("lvert", "|"),
    ("mapsto", "↦"),
    ("mid", "∣"),
    ("models", "⊨"),
    ("mp", "∓"),
    ("mu", "μ"),
    ("nabla", "∇"),
    ("ne", "≠"),
    ("nearrow", "↗"),
    ("neg", "¬"),
    ("neq", "≠"),
    ("nexists", "∄"),
    ("ni", "∋"),
    ("notin", "∉"),
    ("nu", "ν"),
    ("nvDash", "⊭"),
    ("nvdash", "⊬"),
    ("nwarrow", "↖"),
    ("odot", "⊙"),
    ("oint", "∮"),
    ("omega", "ω"),
    ("ominus", "⊖"),
    ("oplus", "⊕"),
    ("oslash", "⊘"),
    ("otimes", "⊗"),
    ("parallel", "∥"),
    ("partial", "∂"),
    ("perp", "⊥"),
    ("phi", "ϕ"),
    ("pi", "π"),
    ("pm", "±"),
    ("prec", "≺"),
    ("preceq", "≼"),
    ("prime", "′"),
    ("prod", "∏"),
    ("propto", "∝"),
    ("psi", "ψ"),
    ("rVert", "‖"),
    ("rangle", "⟩"),
    ("rbrace", "}"),
    ("rceil", "⌉"),
    ("rfloor", "⌋"),
    ("rho", "ρ"),
    ("rightarrow", "→"),
    ("rightharpoondown", "⇁"),
    ("rightharpoonup", "⇀"),
    ("rightleftharpoons", "⇌"),
    ("rightouterjoin", "⟖"),
    ("rightsquigarrow", "⇝"),
    ("rtimes", "⋊"),
    ("rvert", "|"),
    ("searrow", "↘"),
    ("setminus", "∖"),
    ("sigma", "σ"),
    ("sim", "∼"),
    ("simeq", "≃"),
    ("sqcap", "⊓"),
    ("sqcup", "⊔"),
    ("sqsubset", "⊏"),
    ("sqsubseteq", "⊑"),
    ("sqsupset", "⊐"),
    ("sqsupseteq", "⊒"),
    ("square", "□"),
    ("star", "⋆"),
    ("subset", "⊂"),
    ("subseteq", "⊆"),
    ("succ", "≻"),
    ("succeq", "≽"),
    ("sum", "∑"),
    ("supset", "⊃"),
    ("supseteq", "⊇"),
    ("swarrow", "↙"),
    ("tau", "τ"),
    ("therefore", "∴"),
    ("theta", "θ"),
    ("times", "×"),
    ("to", "→"),
    ("top", "⊤"),
    ("triangle", "△"),
    ("triangleleft", "◁"),
    ("triangleright", "▷"),
    ("twoheadleftarrow", "↞"),
    ("twoheadrightarrow", "↠"),
    ("uparrow", "↑"),
    ("uplus", "⊎"),
    ("upsilon", "υ"),
    ("varepsilon", "ε"),
    ("varkappa", "ϰ"),
    ("varnothing", "∅"),
    ("varphi", "φ"),
    ("varpi", "ϖ"),
    ("varrho", "ϱ"),
    ("varsigma", "ς"),
    ("vartheta", "ϑ"),
    ("vdash", "⊢"),
    ("vdots", "⋮"),
    ("vee", "∨"),
    ("vert", "|"),
    ("wedge", "∧"),
    ("wp", "℘"),
    ("wr", "≀"),
    ("xi", "ξ"),
    ("zeta", "ζ"),
];

pub(super) fn symbol(k: &str) -> Option<&'static str> {
    SYMBOLS
        .binary_search_by_key(&k, |&(name, _)| name)
        .ok()
        .map(|idx| SYMBOLS[idx].1)
}

const NAMED_OPERATORS: &[&str] = &[
    "Pr", "arccos", "arcsin", "arctan", "arg", "cos", "cosh", "cot", "coth", "csc", "deg", "det", "dim", "exp", "gcd",
    "hom", "inf", "ker", "lg", "lim", "liminf", "limsup", "ln", "log", "max", "min", "sec", "sin", "sinh", "sup",
    "tan", "tanh",
];

const LIMIT_OPERATORS: &[&str] = &[
    "argmax", "argmin", "inf", "injlim", "lim", "liminf", "limsup", "max", "min", "projlim", "sup",
];

const DISPLAY_LIMIT_SYMBOLS: &[&str] = &[
    "bigcap",
    "bigcup",
    "bigodot",
    "bigoplus",
    "bigotimes",
    "bigsqcup",
    "biguplus",
    "bigvee",
    "bigwedge",
    "coprod",
    "iiint",
    "iint",
    "int",
    "oint",
    "prod",
    "sum",
];

pub(super) fn is_named_operator(k: &str) -> bool {
    NAMED_OPERATORS.binary_search(&k).is_ok()
}

pub(super) fn is_limit_operator(k: &str) -> bool {
    LIMIT_OPERATORS.binary_search(&k).is_ok()
}

pub(super) fn is_display_limit_symbol(k: &str) -> bool {
    DISPLAY_LIMIT_SYMBOLS.binary_search(&k).is_ok()
}

pub(super) fn is_relation(k: &str) -> bool {
    matches!(
        k,
        "Leftarrow"
            | "Leftrightarrow"
            | "Longleftarrow"
            | "Longleftrightarrow"
            | "Longrightarrow"
            | "Rightarrow"
            | "Join"
            | "Vdash"
            | "Vvdash"
            | "approx"
            | "asymp"
            | "bowtie"
            | "cong"
            | "dashv"
            | "fullouterjoin"
            | "doteq"
            | "downarrow"
            | "equiv"
            | "ge"
            | "geq"
            | "geqslant"
            | "gets"
            | "gg"
            | "hookleftarrow"
            | "hookrightarrow"
            | "iff"
            | "implies"
            | "in"
            | "leadsto"
            | "le"
            | "leftarrow"
            | "leftharpoondown"
            | "leftharpoonup"
            | "leftrightarrow"
            | "leftrightharpoons"
            | "leftouterjoin"
            | "leq"
            | "leqslant"
            | "ll"
            | "longleftarrow"
            | "longleftrightarrow"
            | "longmapsto"
            | "longrightarrow"
            | "ltimes"
            | "mapsto"
            | "mid"
            | "models"
            | "ne"
            | "nearrow"
            | "neq"
            | "ni"
            | "notin"
            | "nvdash"
            | "nvDash"
            | "nwarrow"
            | "parallel"
            | "perp"
            | "prec"
            | "preceq"
            | "propto"
            | "rightharpoondown"
            | "rightharpoonup"
            | "rightleftharpoons"
            | "rightouterjoin"
            | "rightarrow"
            | "rightsquigarrow"
            | "rtimes"
            | "searrow"
            | "sim"
            | "simeq"
            | "sqsubset"
            | "sqsubseteq"
            | "sqsupset"
            | "sqsupseteq"
            | "subset"
            | "subseteq"
            | "succ"
            | "succeq"
            | "supset"
            | "supseteq"
            | "swarrow"
            | "to"
            | "triangleleft"
            | "triangleright"
            | "twoheadleftarrow"
            | "twoheadrightarrow"
            | "uparrow"
            | "vdash"
    )
}

const NEGATED: &[(&str, &str)] = &[
    ("<", "≮"),
    (">", "≯"),
    ("=", "≠"),
    ("∈", "∉"),
    ("∋", "∌"),
    ("∣", "∤"),
    ("∥", "∦"),
    ("∼", "≁"),
    ("≃", "≄"),
    ("≅", "≇"),
    ("≈", "≉"),
    ("≡", "≢"),
    ("≤", "≰"),
    ("≥", "≱"),
    ("≺", "⊀"),
    ("≻", "⊁"),
    ("⊂", "⊄"),
    ("⊃", "⊅"),
    ("⊆", "⊈"),
    ("⊇", "⊉"),
    ("⊢", "⊬"),
    ("⊨", "⊭"),
    ("↔", "↮"),
    ("←", "↚"),
    ("→", "↛"),
    ("⇒", "⇏"),
    ("⇐", "⇍"),
    ("⇔", "⇎"),
    ("≼", "⋠"),
    ("≽", "⋡"),
];

pub(super) fn negated(k: &str) -> Option<&'static str> {
    NEGATED.iter().find(|&&(s, _)| s == k).map(|&(_, v)| v)
}

pub(super) fn blackboard(c: char) -> Option<&'static str> {
    Some(match c {
        'C' => "ℂ",
        'H' => "ℍ",
        'N' => "ℕ",
        'P' => "ℙ",
        'Q' => "ℚ",
        'R' => "ℝ",
        'Z' => "ℤ",
        _ => return None,
    })
}

const SUPERSCRIPT: &[(char, &str)] = &[
    ('0', "⁰"),
    ('1', "¹"),
    ('2', "²"),
    ('3', "³"),
    ('4', "⁴"),
    ('5', "⁵"),
    ('6', "⁶"),
    ('7', "⁷"),
    ('8', "⁸"),
    ('9', "⁹"),
    ('+', "⁺"),
    ('-', "⁻"),
    ('=', "⁼"),
    ('(', "⁽"),
    (')', "⁾"),
    ('a', "ᵃ"),
    ('b', "ᵇ"),
    ('c', "ᶜ"),
    ('d', "ᵈ"),
    ('e', "ᵉ"),
    ('f', "ᶠ"),
    ('g', "ᵍ"),
    ('h', "ʰ"),
    ('i', "ⁱ"),
    ('j', "ʲ"),
    ('k', "ᵏ"),
    ('l', "ˡ"),
    ('m', "ᵐ"),
    ('n', "ⁿ"),
    ('o', "ᵒ"),
    ('p', "ᵖ"),
    ('r', "ʳ"),
    ('s', "ˢ"),
    ('t', "ᵗ"),
    ('u', "ᵘ"),
    ('v', "ᵛ"),
    ('w', "ʷ"),
    ('x', "ˣ"),
    ('y', "ʸ"),
    ('z', "ᶻ"),
];

const SUBSCRIPT: &[(char, &str)] = &[
    ('0', "₀"),
    ('1', "₁"),
    ('2', "₂"),
    ('3', "₃"),
    ('4', "₄"),
    ('5', "₅"),
    ('6', "₆"),
    ('7', "₇"),
    ('8', "₈"),
    ('9', "₉"),
    ('+', "₊"),
    ('-', "₋"),
    ('=', "₌"),
    ('(', "₍"),
    (')', "₎"),
    ('a', "ₐ"),
    ('e', "ₑ"),
    ('h', "ₕ"),
    ('i', "ᵢ"),
    ('j', "ⱼ"),
    ('k', "ₖ"),
    ('l', "ₗ"),
    ('m', "ₘ"),
    ('n', "ₙ"),
    ('o', "ₒ"),
    ('p', "ₚ"),
    ('r', "ᵣ"),
    ('s', "ₛ"),
    ('t', "ₜ"),
    ('u', "ᵤ"),
    ('v', "ᵥ"),
    ('x', "ₓ"),
];

fn lookup_char(table: &[(char, &'static str)], c: char) -> Option<&'static str> {
    table.iter().find(|&&(ch, _)| ch == c).map(|&(_, v)| v)
}

pub(super) fn superscript(c: char) -> Option<&'static str> {
    lookup_char(SUPERSCRIPT, c)
}

pub(super) fn subscript(c: char) -> Option<&'static str> {
    lookup_char(SUBSCRIPT, c)
}

pub(super) fn is_spacing(k: &str) -> bool {
    matches!(
        k,
        "," | ":" | ";" | " " | ">" | "enspace" | "enskip" | "medspace" | "quad" | "qquad" | "thickspace" | "thinspace"
    )
}

pub(super) fn is_negative_spacing(k: &str) -> bool {
    matches!(k, "!" | "negmedspace" | "negthickspace" | "negthinspace")
}

pub(super) fn is_font_switch(k: &str) -> bool {
    matches!(k, "bf" | "cal" | "it" | "rm" | "sf" | "sl" | "tt")
}

pub(super) fn is_ignored(k: &str) -> bool {
    matches!(
        k,
        "displaystyle" | "limits" | "nolimits" | "scriptstyle" | "scriptscriptstyle" | "textstyle"
    )
}

pub(super) fn is_size(k: &str) -> bool {
    matches!(
        k,
        "big" | "Big" | "bigg" | "Bigg" | "bigl" | "Bigl" | "biggl" | "Biggl" | "bigr" | "Bigr" | "biggr" | "Biggr"
    )
}

pub(super) fn is_plain_wrapper(k: &str) -> bool {
    matches!(
        k,
        "emph"
            | "mathcal"
            | "mathbf"
            | "mathfrak"
            | "mathit"
            | "mathrm"
            | "mathnormal"
            | "mathscr"
            | "mathsf"
            | "mathtt"
            | "mathup"
            | "mbox"
            | "overbrace"
            | "pmb"
            | "smash"
            | "substack"
            | "text"
            | "textbf"
            | "textit"
            | "textmd"
            | "textnormal"
            | "textrm"
            | "textsc"
            | "textsf"
            | "textsl"
            | "texttt"
            | "textup"
            | "underbrace"
            | "bm"
            | "boldsymbol"
    )
}

const ACCENTS: &[(&str, &str)] = &[
    ("acute", "\u{301}"),
    ("bar", "\u{305}"),
    ("breve", "\u{306}"),
    ("check", "\u{30c}"),
    ("ddot", "\u{308}"),
    ("dot", "\u{307}"),
    ("grave", "\u{300}"),
    ("hat", "\u{302}"),
    ("mathring", "\u{30a}"),
    ("overleftarrow", "\u{20d6}"),
    ("overleftrightarrow", "\u{20e1}"),
    ("overline", "\u{305}"),
    ("overrightarrow", "\u{20d7}"),
    ("tilde", "\u{303}"),
    ("underline", "\u{332}"),
    ("vec", "\u{20d7}"),
    ("widehat", "\u{302}"),
    ("widetilde", "\u{303}"),
];

pub(super) fn accent(k: &str) -> Option<&'static str> {
    ACCENTS.iter().find(|&&(s, _)| s == k).map(|&(_, v)| v)
}
