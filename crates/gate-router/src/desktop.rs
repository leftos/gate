//! The repo's desktop declaration, `.gate/desktop.toml`, read from its bytes ([`read_desktop`]): the `-like` patterns
//! of the routed commands that need the builder's desktop. Nothing here reads a file; the caller hands over its bytes.
//!
//! The file holds three optional keys, at least one of them: `patterns`, the commands that hold the builder's desktop
//! lock for their run ([`DesktopMode::Locked`]); `session`, the commands that need only the builder's console session
//! and run beside other runs without the lock ([`DesktopMode::Session`]); and `hidden`, the commands that run in the
//! console session on a desktop of their own that no one sees, without the lock ([`DesktopMode::Hidden`]). Each is an
//! array of at most [`MOST_PATTERNS`] non-empty strings, each a valid `-like` pattern ([`Pattern`]); an empty array
//! declares none. Any other key is a problem, and so is a pattern listed under any two keys. The reader stops at the
//! first problem it finds:
//! each key's count of entries is checked against the cap before any of its entries, then the entries in file order.
//! A declaration the gate cannot read keeps every routed command of the repo here.

use crate::like::{Pattern, eq_ignoring_case};
use crate::list::{not_a_key, pattern_of, read_entries, table_of};

/// The most patterns a desktop file may declare under each of its keys.
pub const MOST_PATTERNS: usize = 32;

/// How a routed run uses the builder's desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopMode {
    /// The run holds the builder-wide desktop lock for its whole run: a command the `patterns` key declares.
    Locked,
    /// The run needs only the builder's console session and never takes the desktop lock: a command the `session` key
    /// declares.
    Session,
    /// The run needs the builder's console session and runs there on a desktop of its own that no one sees, never
    /// taking the desktop lock: a command the `hidden` key declares, or any consented desktop command a
    /// `--desktop hidden` run gives.
    Hidden,
}

/// What a desktop file declares: the patterns of each [`DesktopMode`], each list in file order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesktopDeclaration {
    /// The `patterns` key's patterns: commands that hold the desktop lock.
    pub patterns: Vec<Pattern>,
    /// The `session` key's patterns: commands that run in the console session without the lock.
    pub session: Vec<Pattern>,
    /// The `hidden` key's patterns: commands that run in the console session on a desktop of their own that no one
    /// sees, without the lock.
    pub hidden: Vec<Pattern>,
}

/// What the desktop file whose bytes are `bytes` declares, each list in file order, a leading byte order mark skipped.
///
/// # Errors
///
/// The file's first problem, such as `patterns[1] is empty`: bytes that are not UTF-8 or not TOML, a key other than
/// `patterns`, `session` and `hidden`, none of them, a value that is not an array, an entry that is not a string, is
/// empty or is no valid wildcard pattern, more than [`MOST_PATTERNS`] entries under one key, or a pattern under two
/// keys.
pub fn read_desktop(bytes: &[u8]) -> Result<DesktopDeclaration, String> {
    let (mut patterns, mut session, mut hidden) = (None, None, None);
    for (name, value) in table_of(bytes)? {
        let slot = match name.as_str() {
            "patterns" => &mut patterns,
            "session" => &mut session,
            "hidden" => &mut hidden,
            _ => return Err(not_a_key(&name, KEYS)),
        };
        let key = name.as_str();
        *slot = Some(read_entries(value, key, MOST_PATTERNS, &|index, text: &str| {
            pattern_of(key, index, text)
        })?);
    }
    if patterns.is_none() && session.is_none() && hidden.is_none() {
        return Err(format!("declares none of the keys ({KEYS})"));
    }
    let declaration = DesktopDeclaration {
        patterns: patterns.unwrap_or_default(),
        session: session.unwrap_or_default(),
        hidden: hidden.unwrap_or_default(),
    };
    for (first, second, key) in [
        (&declaration.patterns, &declaration.session, "patterns and session"),
        (&declaration.patterns, &declaration.hidden, "patterns and hidden"),
        (&declaration.session, &declaration.hidden, "session and hidden"),
    ] {
        if let Some(both) = in_both(first, second) {
            return Err(format!("{:?} is in both {key}", both.as_str()));
        }
    }
    Ok(declaration)
}

/// The keys a desktop file may hold, as its problems name them.
const KEYS: &str = "patterns, session, hidden";

/// The first of `second`'s patterns that `first` lists too, compared with the case folding `-like` matching uses: two
/// such patterns match the same commands.
fn in_both<'a>(first: &[Pattern], second: &'a [Pattern]) -> Option<&'a Pattern> {
    second
        .iter()
        .find(|pattern| first.iter().any(|other| eq_ignoring_case(other.as_str(), pattern.as_str())))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a fixture that does not read must fail loudly")]
mod tests {
    use super::read_desktop;
    use crate::like::Pattern;

    fn strings(patterns: &[Pattern]) -> Vec<String> {
        patterns.iter().map(|pattern| pattern.as_str().to_owned()).collect()
    }

    fn texts(bytes: &[u8]) -> Vec<String> {
        strings(&read_desktop(bytes).expect("the declaration reads").patterns)
    }

    fn session_texts(bytes: &[u8]) -> Vec<String> {
        strings(&read_desktop(bytes).expect("the declaration reads").session)
    }

    fn hidden_texts(bytes: &[u8]) -> Vec<String> {
        strings(&read_desktop(bytes).expect("the declaration reads").hidden)
    }

    #[test]
    fn hidden_patterns_read_beside_the_others_or_alone() {
        let all = b"patterns = ['a*']\nsession = ['b*']\nhidden = ['c*', 'd*']\n";
        assert_eq!(texts(all), ["a*"]);
        assert_eq!(session_texts(all), ["b*"]);
        assert_eq!(hidden_texts(all), ["c*", "d*"]);
        assert_eq!(hidden_texts(b"hidden = ['c*']\n"), ["c*"]);
        assert_eq!(
            texts(b"hidden = ['c*']\n"),
            Vec::<String>::new(),
            "hidden alone declares no locked pattern"
        );
        assert_eq!(hidden_texts(b"patterns = ['a*']\n"), Vec::<String>::new());
    }

    #[test]
    fn a_pattern_in_hidden_and_another_key_is_a_problem_naming_both() {
        assert_eq!(
            problem(b"patterns = ['a*', 'b*']\nhidden = ['c*', 'B*']\n"),
            "\"B*\" is in both patterns and hidden"
        );
        assert_eq!(problem(b"session = ['s*']\nhidden = ['s*']\n"), "\"s*\" is in both session and hidden");
    }

    #[test]
    fn a_bad_hidden_entry_is_a_problem_named_by_its_key_and_other_keys_name_hidden() {
        assert_eq!(problem(b"hidden = ['a', '']\n"), "hidden[1] is empty");
        let list = (0..33).map(|index| format!("'p{index}'")).collect::<Vec<_>>().join(", ");
        assert_eq!(problem(format!("hidden = [{list}]\n").as_bytes()), "declares more than 32 hidden");
        assert_eq!(problem(b"other = 1\nhidden = ['a']\n"), "other is not a key (patterns, session, hidden)");
        assert_eq!(problem(b""), "declares none of the keys (patterns, session, hidden)");
    }

    #[test]
    fn session_patterns_read_beside_patterns_or_alone() {
        let both = b"patterns = ['a*']\nsession = ['b*', 'c*']\n";
        assert_eq!(texts(both), ["a*"]);
        assert_eq!(session_texts(both), ["b*", "c*"]);
        assert_eq!(session_texts(b"session = ['b*']\n"), ["b*"]);
        assert_eq!(texts(b"session = ['b*']\n"), Vec::<String>::new(), "no patterns declares no locked one");
        assert_eq!(session_texts(b"patterns = ['a*']\n"), Vec::<String>::new());
    }

    #[test]
    fn a_pattern_in_both_lists_is_a_problem() {
        assert_eq!(
            problem(b"patterns = ['a*', 'b*']\nsession = ['c*', 'b*']\n"),
            "\"b*\" is in both patterns and session"
        );
        assert_eq!(
            problem(b"patterns = ['Tool *']\nsession = ['tool *']\n"),
            "\"tool *\" is in both patterns and session",
            "a pattern matches without regard to case, so one differing only in case is in both"
        );
        assert_eq!(
            problem("patterns = ['Éditeur *']\nsession = ['éditeur *']\n".as_bytes()),
            "\"éditeur *\" is in both patterns and session",
            "the folding is the matcher's, beyond ASCII"
        );
    }

    #[test]
    fn a_bad_session_entry_is_a_problem_named_by_its_key() {
        for (text, why) in [
            ("session = 'a*'\n", "session is not an array (TOML string)"),
            ("session = ['a*', 3]\n", "session[1] is not a string (TOML integer)"),
            ("session = ['a', '']\n", "session[1] is empty"),
            ("session = ['a[']\n", "session[0] is not a valid wildcard pattern: \"a[\""),
        ] {
            assert_eq!(problem(text.as_bytes()), why, "{text:?}");
        }
        let list = (0..33).map(|index| format!("'p{index}'")).collect::<Vec<_>>().join(", ");
        assert_eq!(problem(format!("session = [{list}]\n").as_bytes()), "declares more than 32 session");
    }

    fn problem(bytes: &[u8]) -> String {
        read_desktop(bytes).expect_err("the declaration has a problem")
    }

    #[test]
    fn the_patterns_read_in_order_and_an_empty_list_declares_none() {
        assert_eq!(
            texts(b"patterns = ['dotnet test *', 'Tool*', '[ab]x']\n"),
            ["dotnet test *", "Tool*", "[ab]x"]
        );
        assert_eq!(texts(b"patterns = []\n"), Vec::<String>::new());
    }

    #[test]
    fn a_byte_order_mark_is_skipped() {
        assert_eq!(texts(b"\xef\xbb\xbfpatterns = ['a*']\n"), ["a*"]);
    }

    #[test]
    fn more_than_thirty_two_patterns_is_a_problem() {
        let list = |count: usize| (0..count).map(|index| format!("'p{index}'")).collect::<Vec<_>>().join(", ");
        assert_eq!(texts(format!("patterns = [{}]\n", list(32)).as_bytes()).len(), 32);
        assert_eq!(
            problem(format!("patterns = [{}]\n", list(33)).as_bytes()),
            "declares more than 32 patterns"
        );
        assert_eq!(
            problem(format!("patterns = ['', {}]\n", list(32)).as_bytes()),
            "declares more than 32 patterns",
            "the cap is checked before any entry"
        );
    }

    #[test]
    fn a_file_that_is_not_toml_or_utf8_is_a_problem() {
        let why = problem(b"patterns = ['a'\n");
        assert!(why.starts_with("line "), "{why}");
        let why = problem(b"\xef\xbb\xbfpatterns = 'a' x\n");
        assert!(why.starts_with("line 1, column 16: "), "{why}");
        let why = problem(b"patterns = ['\xff']\n");
        assert!(why.starts_with("not valid UTF-8 ("), "{why}");
    }

    #[test]
    fn each_bad_shape_or_entry_is_a_problem_and_the_first_wins() {
        for (text, why) in [
            ("", "declares none of the keys (patterns, session, hidden)"),
            ("# none\n", "declares none of the keys (patterns, session, hidden)"),
            ("patterns = 'a*'\n", "patterns is not an array (TOML string)"),
            ("patterns = ['a*', 3]\n", "patterns[1] is not a string (TOML integer)"),
            ("patterns = ['a*', ['b']]\n", "patterns[1] is not a string (TOML array)"),
            ("patterns = ['', 3]\n", "patterns[0] is empty"),
            ("patterns = ['a[']\n", "patterns[0] is not a valid wildcard pattern: \"a[\""),
            ("other = 1\npatterns = ['a']\n", "other is not a key (patterns, session, hidden)"),
            ("patterns = ['a']\n\"b c\" = 1\n", "\"b c\" is not a key (patterns, session, hidden)"),
        ] {
            assert_eq!(problem(text.as_bytes()), why, "{text:?}");
        }
    }
}
