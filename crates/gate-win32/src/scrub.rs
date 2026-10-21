//! A command line made fit for the journal: its secrets redacted, its control, line-separating and direction-overriding
//! characters made spaces, then cut to the length a release keeps, in that order, so no part of a secret the cut would
//! split survives it.

use std::path::Path;

/// The characters of a command line a release keeps.
pub(crate) const COMMAND_CHARS: usize = 200;

/// The characters of a program's path a release keeps, the end of the path, where its file name is.
pub(crate) const IMAGE_CHARS: usize = 1_024;

/// What a redacted secret is written as.
const HIDDEN: &str = "***";

/// The flags, lower-cased, whose next argument is a secret, whatever the program; the single-dash forms are nuget's.
const SECRET_FLAGS: [&str; 7] = ["--token", "--password", "--api-key", "--apikey", "--secret", "-apikey", "-password"];

/// The programs, by file stem, whose `-k <key>` carries a secret; elsewhere `-k` means other things.
const KEY_PROGRAMS: [&str; 2] = ["dotnet", "nuget"];

/// The programs, by file stem, whose glued `-p<password>` carries a secret; elsewhere `-p` means other things, such as
/// `cargo -p <crate>` or `psql -p5432`.
const GLUED_PASSWORD_PROGRAMS: [&str; 4] = ["mysql", "mysqldump", "mariadb", "mariadb-dump"];

/// The auth schemes, lower-cased, that an `Authorization:` names before its credential.
const AUTH_SCHEMES: [&str; 5] = ["basic", "bearer", "token", "digest", "negotiate"];

/// What the key of a `key=value` pair holds, lower-cased, when its value is a secret.
const SECRET_KEYS: [&str; 9] = ["token", "secret", "passw", "pwd", "apikey", "api-key", "api_key", "auth", "credential"];

/// How the word of a known kind of token begins.
const TOKEN_PREFIXES: [&str; 13] = [
    "ghp_",
    "gho_",
    "ghs_",
    "ghu_",
    "github_pat_",
    "glpat-",
    "xoxa-",
    "xoxb-",
    "xoxp-",
    "xoxr-",
    "sk-",
    "AKIA",
    "npm_",
];

/// `line`, the command line of a process running the program whose file stem is `program` (such as `dotnet`; empty when
/// it is unknown, and then argv\[0\]'s stem stands for it), as the journal keeps it: its secrets redacted to `***`
/// ([`redact`]), every control, line-separating and direction-overriding character a space ([`sanitize`]), then cut
/// to [`COMMAND_CHARS`] characters.
pub(crate) fn journal_text(program: &str, line: &str) -> String {
    sanitize(&redact(program, line, 0)).chars().take(COMMAND_CHARS).collect()
}

/// `path`, a program's full path, as the journal keeps it: sanitized ([`sanitize`]), then cut to its last
/// [`IMAGE_CHARS`] characters, so its file name survives the cut.
pub(crate) fn journal_image(path: &str) -> String {
    let sanitized = sanitize(path);
    let count = sanitized.chars().count();
    sanitized.chars().skip(count.saturating_sub(IMAGE_CHARS)).collect()
}

/// `text` with every C0 control, DEL and C1 control, the line and paragraph separators (U+2028, U+2029) and the
/// direction embeddings, overrides and isolates (U+202A to U+202E, U+2066 to U+2069) made spaces, so the text reprints
/// as itself on a terminal or a page.
pub(crate) fn sanitize(text: &str) -> String {
    text.chars().map(|character| if is_unsafe(character) { ' ' } else { character }).collect()
}

/// Whether `character` is one [`sanitize`] makes a space.
fn is_unsafe(character: char) -> bool {
    // The general category Cc is exactly C0, DEL and C1.
    character.is_control() || matches!(character, '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// One piece of a command line: an argument, its quotes kept, or the white space between two.
enum Piece<'a> {
    Argument(&'a str),
    Space(&'a str),
}

/// What the argument after one is, as the one before it says.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Next {
    /// An argument like any other.
    Plain,
    /// A secret, hidden whole.
    Secret,
    /// A `user:password`, whose password is hidden.
    UserPassword,
}

/// Which of the rules that hold for some programs only hold for the program of a command line.
#[derive(Clone, Copy)]
struct Rules {
    /// `-k <key>` carries a secret ([`KEY_PROGRAMS`]).
    key: bool,
    /// A glued `-p<password>` carries a secret ([`GLUED_PASSWORD_PROGRAMS`]).
    glued_password: bool,
    /// The program is `cmd`, so the rest of the line after `/c` or `/k` is a command line of its own.
    shell: bool,
}

impl Rules {
    /// The rules for `line`, run by the program whose file stem is `program`, or by argv\[0\]'s when it is empty.
    fn of(program: &str, line: &str) -> Rules {
        let stem = if program.is_empty() {
            first_stem(line)
        } else {
            program.to_ascii_lowercase()
        };
        Rules {
            key: KEY_PROGRAMS.contains(&stem.as_str()),
            glued_password: GLUED_PASSWORD_PROGRAMS.contains(&stem.as_str()),
            shell: stem == "cmd",
        }
    }
}

/// The file stem of `line`'s first argument, its quotes taken off, lower-cased; empty when it has none.
fn first_stem(line: &str) -> String {
    let first = pieces(line).into_iter().find_map(|piece| match piece {
        Piece::Argument(argument) => Some(argument),
        Piece::Space(_) => None,
    });
    first
        .and_then(|argument| Path::new(argument.trim_matches('"')).file_stem())
        .map(|stem| stem.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// `line`, the command line of the program `program`, with its secrets hidden: a URL's credentials, keeping a user
/// before a password; the argument after each of [`SECRET_FLAGS`]; the argument after `-k` for [`KEY_PROGRAMS`] and
/// the rest of a glued `-p<password>` for [`GLUED_PASSWORD_PROGRAMS`]; the password of the `user:password` after `-u`,
/// a drive path excepted; the rest of an argument after `Authorization:` or `Bearer `, or the next argument when one
/// ends there, and the one after an auth scheme there; the value of each `key=value` pair whose key names a secret
/// ([`SECRET_KEYS`]); and each word beginning as a known kind of token does ([`TOKEN_PREFIXES`]). An argument quoted
/// around white space, at `depth` 0, is redacted as a command line of its own first, and so is the rest of a `cmd` line
/// after `/c` or `/k`, its program the stem of its own first word.
fn redact(program: &str, line: &str, depth: usize) -> String {
    let mut redacted = String::with_capacity(line.len());
    let mut closings = Vec::new();
    let mut program = program;
    let mut line = line;
    while let Some(rest) = redact_to_shell_switch(program, line, depth, &mut redacted) {
        let (opening, inner, closing) = unwrapped(rest);
        redacted.push_str(opening);
        closings.push(closing);
        program = "";
        line = inner;
    }
    for closing in closings.into_iter().rev() {
        redacted.push_str(closing);
    }
    redacted
}

/// Writes `line`, the command line of the program `program` at `depth`, to `redacted` with its secrets hidden, up to
/// and with a `cmd` line's `/c` or `/k`, and gives the rest of the line after that switch; `None`, the whole line
/// written, when it has no such switch.
fn redact_to_shell_switch<'a>(program: &str, line: &'a str, depth: usize, redacted: &mut String) -> Option<&'a str> {
    let rules = Rules::of(program, line);
    let mut next = Next::Plain;
    let mut end = 0;
    for piece in pieces(line) {
        match piece {
            Piece::Space(space) => {
                redacted.push_str(space);
                end += space.len();
            }
            Piece::Argument(argument) => {
                end += argument.len();
                if rules.shell && next == Next::Plain && is_shell_switch(argument) {
                    redacted.push_str(argument);
                    return Some(&line[end..]);
                }
                let (text, after) = redact_argument(argument, next, rules, depth);
                redacted.push_str(&text);
                next = after;
            }
        }
    }
    None
}

/// Whether `argument` is `cmd`'s `/c` or `/k`, in any case.
fn is_shell_switch(argument: &str) -> bool {
    argument.eq_ignore_ascii_case("/c") || argument.eq_ignore_ascii_case("/k")
}

/// `rest`, the text after a `cmd` line's `/c` or `/k`, as the white space and quote before the command line it runs,
/// that command line, and the quote and white space after it: when `rest`, white space aside, begins and ends with a
/// double quote, `cmd` strips both and runs what lies between; otherwise it runs `rest` as it stands.
fn unwrapped(rest: &str) -> (&str, &str, &str) {
    let body = rest.trim();
    let start = rest.len() - rest.trim_start().len();
    let end = start + body.len();
    if body.len() >= 2 && body.starts_with('"') && body.ends_with('"') {
        (&rest[..=start], &rest[start + 1..end - 1], &rest[end - 1..])
    } else {
        (&rest[..start], &rest[start..], "")
    }
}

/// The pieces of `line`: each run of white space outside double quotes is a space, and each run between two an
/// argument. A double quote after an odd run of backslashes is a literal one, as `CommandLineToArgvW` reads it.
fn pieces(line: &str) -> Vec<Piece<'_>> {
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut backslashes = 0_usize;
    let mut in_space: Option<bool> = None;
    for (index, character) in line.char_indices() {
        let space = !quoted && character.is_whitespace();
        if character == '"' && backslashes.is_multiple_of(2) {
            quoted = !quoted;
        }
        backslashes = if character == '\\' { backslashes + 1 } else { 0 };
        if in_space.is_some_and(|was| was != space) {
            pieces.push(piece(&line[start..index], !space));
            start = index;
        }
        in_space = Some(space);
    }
    if let Some(space) = in_space {
        pieces.push(piece(&line[start..], space));
    }
    pieces
}

/// `text` as a space when `space`, else as an argument.
fn piece(text: &str, space: bool) -> Piece<'_> {
    if space { Piece::Space(text) } else { Piece::Argument(text) }
}

/// `argument`, which the argument before it said is `this`, of a command line at `depth` whose program `rules` hold,
/// with its secrets hidden, and what it says the next one is.
fn redact_argument(argument: &str, this: Next, rules: Rules, depth: usize) -> (String, Next) {
    let bare = argument.trim_matches('"');
    let lower = bare.to_ascii_lowercase();
    if let Some(redacted) = as_said(argument, &lower, this).or_else(|| flagged(argument, bare, &lower, rules)) {
        return redacted;
    }
    let argument = if depth == 0 && argument.contains(char::is_whitespace) {
        quoted_redacted(argument)
    } else {
        argument.to_owned()
    };
    let (text, next) = header(&argument).unwrap_or_else(|| (pairs(&argument), Next::Plain));
    (tokens(&credentials(&text)), next)
}

/// `argument`, lower-cased and unquoted as `lower`, redacted as the argument before it said it is, `this`; `None` when
/// it said nothing of it, or a `user:password` was said and this is none.
fn as_said(argument: &str, lower: &str, this: Next) -> Option<(String, Next)> {
    match this {
        // An unquoted `Authorization: Basic <credential>` hides the scheme and the credential after it.
        Next::Secret => {
            let next = if AUTH_SCHEMES.contains(&lower) { Next::Secret } else { Next::Plain };
            Some((HIDDEN.to_owned(), next))
        }
        Next::UserPassword => user_password(argument).map(|text| (text, Next::Plain)),
        Next::Plain => None,
    }
}

/// `argument`, unquoted as `bare` and lower-cased as `lower`, when it is a flag whose next argument is a secret or a
/// `user:password`, or a glued password, with what it says the next one is; `None` when it is none of them.
fn flagged(argument: &str, bare: &str, lower: &str, rules: Rules) -> Option<(String, Next)> {
    if SECRET_FLAGS.contains(&lower) || (rules.key && bare == "-k") {
        return Some((argument.to_owned(), Next::Secret));
    }
    if bare == "-u" {
        return Some((argument.to_owned(), Next::UserPassword));
    }
    (rules.glued_password && glued_password(bare, lower)).then(|| (format!("-p{HIDDEN}"), Next::Plain))
}

/// Whether `bare`, lower-cased as `lower`, is a password glued to `-p`; `-p:` and `-property:` set properties.
fn glued_password(bare: &str, lower: &str) -> bool {
    bare.len() > 2 && bare.starts_with("-p") && !bare.starts_with("-p:") && !lower.starts_with("-property:")
}

/// `argument`, the one after `-u`, with the password of its `user:password` hidden and its user scanned as any text
/// is; `None` when it holds no `:` or is a drive path (`C:\…`), which is no password.
fn user_password(argument: &str) -> Option<String> {
    let colon = argument.find(':')?;
    let bytes = argument.trim_start_matches('"').as_bytes();
    let drive = bytes.len() > 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/');
    (!drive).then(|| format!("{}{HIDDEN}", tokens(&credentials(&argument[..=colon]))))
}

/// `argument`, which holds white space inside its quotes, with the text between its first and its last unescaped double
/// quote ([`unescaped_quotes`]), or the end of the argument when it has only one, redacted as a command line of its
/// own, its program the stem of its first word.
fn quoted_redacted(argument: &str) -> String {
    let mut quotes = unescaped_quotes(argument);
    let Some(open) = quotes.next() else {
        return argument.to_owned();
    };
    let close = quotes.last().unwrap_or(argument.len());
    format!(
        "{}\"{}{}",
        &argument[..open],
        redact("", &argument[open + 1..close], 1),
        &argument[close..]
    )
}

/// Where each double quote of `text` that is not a literal one stands: a quote after an odd run of backslashes is a
/// literal one, as [`pieces`] reads it.
fn unescaped_quotes(text: &str) -> impl Iterator<Item = usize> + '_ {
    let mut backslashes = 0_usize;
    text.char_indices().filter_map(move |(index, character)| {
        let quote = character == '"' && backslashes.is_multiple_of(2);
        backslashes = if character == '\\' { backslashes + 1 } else { 0 };
        quote.then_some(index)
    })
}

/// `argument` with the rest of it after `Authorization:` or `Bearer ` hidden, and the next argument a secret when this
/// one ends at `Authorization:` or is `Bearer`; `None` when it holds neither.
fn header(argument: &str) -> Option<(String, Next)> {
    let lower = argument.to_ascii_lowercase();
    let closing = if argument.len() > 1 && argument.ends_with('"') { "\"" } else { "" };
    if lower.trim_matches('"') == "bearer" {
        return Some((argument.to_owned(), Next::Secret));
    }
    for marker in ["authorization:", "bearer "] {
        if let Some(at) = lower.find(marker) {
            let end = at + marker.len();
            if argument[end..].trim_end_matches('"').trim().is_empty() {
                return Some((argument.to_owned(), Next::Secret));
            }
            return Some((format!("{} {HIDDEN}{closing}", argument[..end].trim_end()), Next::Plain));
        }
    }
    None
}

/// Whether `character` ends a key's start: a key begins after it.
fn splits_keys(character: char) -> bool {
    matches!(character, ',' | ';' | '&' | '?' | '"') || character.is_whitespace()
}

/// Where [`pairs`] stands as to a secret value.
#[derive(Clone, Copy)]
enum Value {
    /// Outside one.
    Outside,
    /// In an unquoted one.
    Bare,
    /// At the opening quote of a quoted one.
    Opening,
    /// In a quoted one, after this many backslashes in a row.
    Quoted(usize),
}

impl Value {
    /// Where [`pairs`] stands once it reads `character`, with `after` the text after it, and whether `character` is
    /// hidden.
    fn on(self, character: char, after: &str) -> (Value, bool) {
        match self {
            Value::Outside => (Value::Outside, false),
            Value::Bare if ends_bare_value(character, after) => (Value::Outside, false),
            Value::Bare => (Value::Bare, true),
            Value::Opening => (Value::Quoted(0), false),
            Value::Quoted(backslashes) if character == '"' && backslashes.is_multiple_of(2) => (Value::Outside, false),
            Value::Quoted(backslashes) => (Value::Quoted(if character == '\\' { backslashes + 1 } else { 0 }), true),
        }
    }
}

/// Whether `character`, with `after` the text after it, ends an unquoted value: a `"` or white space does, and a `,`,
/// `;` or `&` when `after` reaches a `=` before the next character that ends a key's start ([`splits_keys`]), so a
/// pair follows it.
fn ends_bare_value(character: char, after: &str) -> bool {
    match character {
        ',' | ';' | '&' => after
            .find(|next: char| next == '=' || splits_keys(next))
            .is_some_and(|at| after[at..].starts_with('=')),
        '"' => true,
        _ => character.is_whitespace(),
    }
}

/// `argument` with the value of each `key=value` pair in it whose key names a secret ([`SECRET_KEYS`]) hidden: a key
/// runs from the argument's start, or from the last `,`, `;`, `&`, `?`, `"` or white space, to its `=`. A value that
/// opens with `"` runs to the next unescaped `"`, or to the argument's end when none closes it, both quotes kept;
/// another runs to the next `"` or white space, or to a `,`, `;` or `&` that another pair follows ([`ends_bare_value`]).
fn pairs(argument: &str) -> String {
    let mut redacted = String::with_capacity(argument.len());
    let mut key_start = 0;
    let mut value = Value::Outside;
    for (index, character) in argument.char_indices() {
        let after = &argument[index + character.len_utf8()..];
        let (now, hidden) = value.on(character, after);
        value = now;
        if hidden {
            continue;
        }
        redacted.push(character);
        if matches!(value, Value::Quoted(_)) {
            // The opening quote of a secret value, written; what it quotes is hidden.
            redacted.push_str(HIDDEN);
        } else if splits_keys(character) {
            key_start = index + character.len_utf8();
        } else if character == '=' && names_secret(&argument[key_start..index]) {
            if after.starts_with('"') {
                value = Value::Opening;
            } else {
                redacted.push_str(HIDDEN);
                value = Value::Bare;
            }
        }
    }
    redacted
}

/// Whether `key`, a pair's key, names a secret ([`SECRET_KEYS`]).
fn names_secret(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    SECRET_KEYS.iter().any(|secret| key.contains(secret))
}

/// `text` with the credentials of each URL in it hidden: `scheme://user:password@` keeps `scheme://user:***@`, and
/// `scheme://token@` keeps `scheme://***@`.
fn credentials(text: &str) -> String {
    let mut redacted = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(scheme_end) = rest.find("://") {
        let authority_start = scheme_end + "://".len();
        redacted.push_str(&rest[..authority_start]);
        let after = &rest[authority_start..];
        let length = after
            .find(|character: char| matches!(character, '/' | '?' | '#' | '"' | '\'') || character.is_whitespace())
            .unwrap_or(after.len());
        let authority = &after[..length];
        match authority.rfind('@') {
            Some(at) => {
                let user = &authority[..at];
                if let Some(colon) = user.find(':') {
                    redacted.push_str(&user[..=colon]);
                }
                redacted.push_str(HIDDEN);
                redacted.push_str(&authority[at..]);
            }
            None => redacted.push_str(authority),
        }
        rest = &after[length..];
    }
    redacted.push_str(rest);
    redacted
}

/// `text` with each word, a run of ASCII letters, digits, `_` and `-`, that begins as a known kind of token does
/// ([`TOKEN_PREFIXES`]) hidden whole.
fn tokens(text: &str) -> String {
    let in_word = |character: char| character.is_ascii_alphanumeric() || character == '_' || character == '-';
    let mut redacted = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(first) = rest.chars().next() {
        let length = rest.find(|character: char| !in_word(character)).unwrap_or(rest.len());
        if length == 0 {
            redacted.push(first);
            rest = &rest[first.len_utf8()..];
            continue;
        }
        let word = &rest[..length];
        redacted.push_str(if TOKEN_PREFIXES.iter().any(|prefix| word.starts_with(prefix)) {
            HIDDEN
        } else {
            word
        });
        rest = &rest[length..];
    }
    redacted
}

#[cfg(test)]
mod tests;
