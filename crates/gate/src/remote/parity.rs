//! The toolchain parity check a routed run makes between its preflight and its push, as the PowerShell client does:
//! whether the builder has the .NET SDK the snapshot's `global.json` pin resolves to under its rollForward policy, or,
//! for a .NET tree with no pin, an SDK of this PC's newest major. Nothing here runs a process or reads a file: the
//! client lists the snapshot's tree, reads its `global.json`, the builder's SDK line and this PC's SDKs, and [`need`]
//! lists what the tree needs that the builder lacks, each a [`Need`] of its [`NeedKind`], which [`lines`] words one
//! mismatch line each, in order. A further toolchain need, such as a workload, is one more kind in that list.

use std::cmp::Ordering;

use serde_json::Value;

/// The line said when the snapshot's `global.json` holds no SDK pin that can be read.
pub(crate) const NO_READABLE_PIN: &str = "gate: global.json has no readable sdk pin; checking the SDK major only";
/// The policy a pin that names none rolls forward by, the SDK's default.
const DEFAULT_ROLL: &str = "patch";

/// An SDK version `x.y.znn` as its parts: `z`, the feature band, and `nn`, the patch, are the hundreds and the rest of
/// the third number (10.0.401: band 4, patch 1); `pre` is the prerelease label after `-`, empty for a release.
#[derive(Clone, Copy, Debug)]
struct SdkVersion<'t> {
    text: &'t str,
    major: u64,
    minor: u64,
    band: u64,
    patch: u64,
    pre: &'t str,
}

impl<'t> SdkVersion<'t> {
    /// The version `text` spells, `x.y.z` of digits with an optional `-<label>` of letters, digits, dots and dashes;
    /// `None` when it is not one.
    fn parse(text: &'t str) -> Option<SdkVersion<'t>> {
        let (release, pre) = match text.split_once('-') {
            Some((release, pre)) if is_label(pre) => (release, pre),
            Some(_) => return None,
            None => (text, ""),
        };
        let mut parts = release.split('.').map(number);
        let (Some(Some(major)), Some(Some(minor)), Some(Some(third)), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            return None;
        };
        Some(SdkVersion {
            text,
            major,
            minor,
            band: third / 100,
            patch: third % 100,
            pre,
        })
    }

    /// Its major, minor and feature band, the parts a rollForward policy keeps a candidate to.
    fn line(&self) -> [u64; 3] {
        [self.major, self.minor, self.band]
    }
}

/// Whether `label` is a prerelease label: one or more letters, digits, dots and dashes.
fn is_label(label: &str) -> bool {
    !label.is_empty() && label.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-')
}

/// The number `text` spells in one or more ASCII digits; `None` when it is not one, or too large.
pub(super) fn number(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Orders two versions by major, minor, band and patch, then by their prerelease labels.
fn compare(left: &SdkVersion<'_>, right: &SdkVersion<'_>) -> Ordering {
    [left.major, left.minor, left.band, left.patch]
        .cmp(&[right.major, right.minor, right.band, right.patch])
        .then_with(|| compare_labels(left.pre, right.pre))
}

/// Orders two prerelease labels as semantic versions do: a release (empty) above any prerelease, then dot-separated
/// parts left to right, numbers by value and below words, words ordinally, and a label that runs out first lower.
fn compare_labels(left: &str, right: &str) -> Ordering {
    if left == right {
        return Ordering::Equal;
    }
    if left.is_empty() {
        return Ordering::Greater;
    }
    if right.is_empty() {
        return Ordering::Less;
    }
    let (mut lefts, mut rights) = (left.split('.'), right.split('.'));
    loop {
        match (lefts.next(), rights.next()) {
            (Some(left), Some(right)) => match compare_label_part(left, right) {
                Ordering::Equal => {}
                order => return order,
            },
            (left, right) => return left.is_some().cmp(&right.is_some()),
        }
    }
}

/// Orders two parts of a prerelease label: numbers by value and below words, words ordinally.
fn compare_label_part(left: &str, right: &str) -> Ordering {
    let numeric = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    match (numeric(left), numeric(right)) {
        (true, true) => {
            let (left, right) = (left.trim_start_matches('0'), right.trim_start_matches('0'));
            left.len().cmp(&right.len()).then_with(|| left.cmp(right))
        }
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => left.cmp(right),
    }
}

/// What each rollForward policy of `global.json` lets dotnet choose, from
/// <https://learn.microsoft.com/dotnet/core/tools/global-json> ("rollForward"), for an SDK version x.y.znn (x major, y
/// minor, z feature band, nn patch):
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Roll {
    /// Uses the specified version. If not found, rolls forward to the latest patch level. If not found, fails. The
    /// default when no rollForward value is set.
    Patch,
    /// Uses the latest patch level for the specified major, minor, and feature band. If not found, rolls forward to the
    /// next higher feature band within the same major/minor and uses the latest patch level for that feature band.
    Feature,
    /// As feature; if not found, rolls forward to the next higher minor and feature band within the same major.
    Minor,
    /// As minor; if not found, rolls forward to the next higher major, minor, and feature band.
    Major,
    /// Uses the latest installed patch level that matches the requested major, minor, and feature band with a patch
    /// level that's greater than or equal to the specified value.
    LatestPatch,
    /// Uses the highest installed feature band and patch level that matches the requested major and minor.
    LatestFeature,
    /// Uses the highest installed minor, feature band, and patch level that matches the requested major.
    LatestMinor,
    /// Uses the highest installed .NET SDK with a version that's greater than or equal to the specified value.
    LatestMajor,
    /// Doesn't roll forward; an exact match is required.
    Disable,
}

/// How a policy picks among its candidates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pick {
    /// The requested version when it is installed, else the highest.
    Exact,
    /// The requested version or none.
    Only,
    /// The highest.
    Highest,
    /// The highest patch of the lowest feature band, with its minor and major.
    Nearest,
}

impl Roll {
    /// The policy `name` names, its letter case aside as the PowerShell client reads it; `None` for one dotnet does not
    /// know.
    fn named(name: &str) -> Option<Roll> {
        const ROLLS: [(&str, Roll); 9] = [
            ("patch", Roll::Patch),
            ("feature", Roll::Feature),
            ("minor", Roll::Minor),
            ("major", Roll::Major),
            ("latestPatch", Roll::LatestPatch),
            ("latestFeature", Roll::LatestFeature),
            ("latestMinor", Roll::LatestMinor),
            ("latestMajor", Roll::LatestMajor),
            ("disable", Roll::Disable),
        ];
        ROLLS.iter().find(|(word, _)| word.eq_ignore_ascii_case(name)).map(|(_, roll)| *roll)
    }

    /// How many of major, minor and feature band a candidate shares with the requested version.
    fn same(self) -> usize {
        match self {
            Roll::Patch | Roll::LatestPatch | Roll::Disable => 3,
            Roll::Feature | Roll::LatestFeature => 2,
            Roll::Minor | Roll::LatestMinor => 1,
            Roll::Major | Roll::LatestMajor => 0,
        }
    }

    fn pick(self) -> Pick {
        match self {
            Roll::Patch => Pick::Exact,
            Roll::Disable => Pick::Only,
            Roll::Feature | Roll::Minor | Roll::Major => Pick::Nearest,
            Roll::LatestPatch | Roll::LatestFeature | Roll::LatestMinor | Roll::LatestMajor => Pick::Highest,
        }
    }
}

/// The SDK dotnet would choose from `available` for a `global.json` pin of `requested` under `roll_forward` (empty for
/// the default, `patch`), considering prerelease SDKs only when `allow_prerelease`; `None` when it would fail, as it
/// does for a version that is not x.y.znn or a policy it does not know. A prerelease pin under `allow_prerelease` false
/// is refused like any prerelease SDK.
fn resolve<'a>(requested: &str, roll_forward: &str, allow_prerelease: bool, available: &'a [String]) -> Option<&'a str> {
    let want = SdkVersion::parse(requested)?;
    let roll = Roll::named(if roll_forward.is_empty() { DEFAULT_ROLL } else { roll_forward })?;
    let candidates: Vec<SdkVersion<'a>> = available
        .iter()
        .filter_map(|text| SdkVersion::parse(text))
        .filter(|version| allow_prerelease || version.pre.is_empty())
        .filter(|version| version.line().iter().take(roll.same()).eq(want.line().iter().take(roll.same())))
        .filter(|version| compare(version, &want) != Ordering::Less)
        .collect();
    select(&candidates, &want, roll.pick())
}

/// The candidate `pick` chooses for `want`; `None` when there is none to choose.
fn select<'a>(candidates: &[SdkVersion<'a>], want: &SdkVersion<'_>, pick: Pick) -> Option<&'a str> {
    let exact = candidates.iter().find(|version| compare(version, want) == Ordering::Equal);
    match (pick, exact) {
        (Pick::Only, _) | (Pick::Exact, Some(_)) => return exact.map(|version| version.text),
        _ => {}
    }
    let chosen = if pick == Pick::Nearest {
        let low = extreme(candidates.iter(), Ordering::Less)?;
        extreme(candidates.iter().filter(|version| version.line() == low.line()), Ordering::Greater)
    } else {
        extreme(candidates.iter(), Ordering::Greater)
    };
    chosen.map(|version| version.text)
}

/// The highest (`Greater`) or lowest (`Less`) of `versions`, the first of equals; `None` when there are none.
fn extreme<'v, 'a>(versions: impl Iterator<Item = &'v SdkVersion<'a>>, toward: Ordering) -> Option<&'v SdkVersion<'a>> {
    versions.fold(None, |best, version| match best {
        Some(best) if compare(version, best) != toward => Some(best),
        _ => Some(version),
    })
}

/// The SDK pin of a snapshot's root `global.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pin {
    /// Whether the snapshot has a `global.json` at its root.
    pub found: bool,
    /// Its `sdk.version`; `None` when the file does not parse, holds no `sdk.version` text, or names a version that is
    /// not the three-part `x.y.z` dotnet reads.
    pub version: Option<String>,
    /// Its `sdk.rollForward`; `None` when it gives none, which rolls forward by [`DEFAULT_ROLL`].
    pub roll_forward: Option<String>,
    /// Its `sdk.allowPrerelease`: true unless given as a JSON `false`, the default outside Visual Studio.
    pub allow_prerelease: bool,
}

impl Pin {
    /// A `global.json` the client could not read whole: it holds no readable pin, so the major rule applies.
    pub(crate) fn unreadable() -> Pin {
        Pin {
            found: true,
            version: None,
            roll_forward: None,
            allow_prerelease: true,
        }
    }
}

/// The pin of the `global.json` whose text is `global_json`, `None` when the snapshot has none. Its comments are
/// stripped as dotnet strips them, its members are spelled exactly as dotnet spells them, a leading byte order mark
/// is skipped, only its first JSON value is read, as dotnet reads it, and any text after it is ignored; a
/// `global.json` dotnet would ignore holds no readable pin.
pub(crate) fn pin_of(global_json: Option<&str>) -> Pin {
    let mut pin = Pin {
        found: global_json.is_some(),
        version: None,
        roll_forward: None,
        allow_prerelease: true,
    };
    let Some(text) = global_json else {
        return pin;
    };
    // A text that is not the file's own bytes — a lossy read of one that is not UTF-8 holds U+FFFD — is no pin.
    if text.contains('\u{fffd}') {
        return pin;
    }
    let stripped = without_comments(text.trim_start_matches('\u{feff}'));
    let mut values = serde_json::Deserializer::from_str(&stripped).into_iter::<Value>();
    let Some(Ok(parsed)) = values.next() else {
        return pin;
    };
    let Some(sdk) = member(&parsed, "sdk") else {
        return pin;
    };
    let Some(version) = member(sdk, "version")
        .and_then(Value::as_str)
        .filter(|version| SdkVersion::parse(version).is_some())
    else {
        return pin;
    };
    let roll_forward = match member(sdk, "rollForward") {
        None => None,
        Some(value) => match value.as_str().filter(|policy| Roll::named(policy).is_some()) {
            Some(policy) => Some(policy.to_owned()),
            None => return pin,
        },
    };
    let allow_prerelease = match member(sdk, "allowPrerelease") {
        None => true,
        Some(Value::Bool(allow)) => *allow,
        Some(_) => return pin,
    };
    pin.version = Some(version.to_owned());
    pin.roll_forward = roll_forward;
    pin.allow_prerelease = allow_prerelease;
    pin
}

/// The text `text` with the JSON comments dotnet reads dropped: `//` to the end of its line, its line end kept, and
/// `/* ... */` each as one space, outside string values; a marker inside a string value is kept, and an unterminated
/// block comment drops the rest.
fn without_comments(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '"' => {
                kept.push('"');
                while let Some(inner) = characters.next() {
                    kept.push(inner);
                    match inner {
                        '\\' => {
                            if let Some(escaped) = characters.next() {
                                kept.push(escaped);
                            }
                        }
                        '"' => break,
                        _ => {}
                    }
                }
            }
            '/' if characters.peek() == Some(&'/') => {
                for inner in characters.by_ref() {
                    if inner == '\n' {
                        kept.push('\n');
                        break;
                    }
                }
            }
            '/' if characters.peek() == Some(&'*') => {
                characters.next();
                let mut star = false;
                for inner in characters.by_ref() {
                    if star && inner == '/' {
                        break;
                    }
                    star = inner == '*';
                }
                kept.push(' ');
            }
            _ => kept.push(character),
        }
    }
    kept
}

/// The member `name` of the JSON object `value`, its name spelled exactly as dotnet spells it; `None` when `value` is
/// no object or has no such member.
fn member<'v>(value: &'v Value, name: &str) -> Option<&'v Value> {
    value.as_object()?.get(name)
}

/// Whether a tree whose paths are `names` is a .NET one: it holds a `global.json`, or a `.sln`, `.slnx` or `.csproj`
/// file, anywhere, the letter case of the names aside.
pub(crate) fn is_dotnet_tree<'n>(names: impl IntoIterator<Item = &'n str>) -> bool {
    names.into_iter().any(|name| {
        let name = name.to_ascii_lowercase();
        name == "global.json" || [".sln", ".slnx", ".csproj", "/global.json"].iter().any(|end| name.ends_with(end))
    })
}

/// The versions an SDK list `listing` names, comma-separated as the box's `gate: remote sdks` line says them, each
/// trimmed, and none for `none` or an empty entry.
pub(crate) fn listed(listing: &str) -> Vec<String> {
    listing
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty() && !entry.eq_ignore_ascii_case("none"))
        .map(str::to_owned)
        .collect()
}

/// The newest major among the SDK versions `versions`; 0 when none is a version.
pub(crate) fn newest_major<'v>(versions: impl IntoIterator<Item = &'v str>) -> u64 {
    versions
        .into_iter()
        .filter_map(SdkVersion::parse)
        .map(|version| version.major)
        .max()
        .unwrap_or(0)
}

/// What a toolchain need is of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NeedKind {
    /// A .NET SDK, which the builder's SDK list serves.
    Sdk,
}

/// One thing a snapshot needs that the builder lacks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Need {
    /// What it is of, which decides what the builder's side of its mismatch line says.
    pub kind: NeedKind,
    /// The need in words, as its mismatch line says it after `needs`.
    pub text: String,
}

/// What a snapshot with `pin` needs that a builder whose SDKs are `available` does not give, in the order the mismatch
/// lines say them; empty when it serves them all. The SDK need is the one [`sdk_need`] words.
pub(crate) fn need(pin: &Pin, available: &[String], local_major: impl FnOnce() -> u64) -> Vec<Need> {
    sdk_need(pin, available, local_major)
        .map(|text| Need { kind: NeedKind::Sdk, text })
        .into_iter()
        .collect()
}

/// The SDK a snapshot with `pin` needs that a builder whose SDKs are `available` does not give; `None` when they serve
/// it. A pin needs the SDK it resolves to: `.NET SDK <version> (rollForward <policy>)`. With no pin, the tree needs an
/// SDK of this PC's newest major, which `local_major` reads only then: `a .NET <major> SDK`; a PC with none (0) needs
/// nothing.
fn sdk_need(pin: &Pin, available: &[String], local_major: impl FnOnce() -> u64) -> Option<String> {
    if let Some(version) = &pin.version {
        let policy = pin.roll_forward.as_deref().unwrap_or(DEFAULT_ROLL);
        return match resolve(version, policy, pin.allow_prerelease, available) {
            Some(_) => None,
            None => Some(format!(".NET SDK {version} (rollForward {policy})")),
        };
    }
    let major = local_major();
    let served = available
        .iter()
        .filter_map(|text| SdkVersion::parse(text))
        .any(|version| version.major == major);
    (major != 0 && !served).then(|| format!("a .NET {major} SDK"))
}

/// The mismatch lines said when the builder `host`, whose SDKs are `available`, lacks the `needs` of `repo`: one per
/// need, in their order, each ending in what the builder has of that need's kind.
pub(crate) fn lines(repo: &str, needs: &[Need], host: &str, available: &[String]) -> Vec<String> {
    needs
        .iter()
        .map(|need| mismatch_line(repo, need, host, &have(need.kind, available)))
        .collect()
}

/// What a builder whose SDKs are `available` has of `kind`, as a mismatch line ends in it: for an SDK, its SDKs
/// comma-separated, `none` when it listed none.
fn have(kind: NeedKind, available: &[String]) -> String {
    match kind {
        NeedKind::Sdk if available.is_empty() => "none".to_owned(),
        NeedKind::Sdk => available.join(", "),
    }
}

/// The line said when the builder `host`, which has `have` of the need's kind, lacks `need` of `repo`.
fn mismatch_line(repo: &str, need: &Need, host: &str, have: &str) -> String {
    format!("gate: remote toolchain mismatch: {repo} needs {}; {host} has {have}", need.text)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a case whose version does not parse must fail loudly")]
mod tests {
    use std::cell::Cell;
    use std::cmp::Ordering;

    use super::{Need, NeedKind, Pin, SdkVersion, compare, have, is_dotnet_tree, lines, listed, mismatch_line, need, newest_major, pin_of, resolve};

    fn owned(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| (*text).to_owned()).collect()
    }

    /// The text of the one SDK need in `needs`, `None` when it holds none; more than one need, or one of another kind,
    /// fails the case.
    fn the_sdk_need(needs: Vec<Need>) -> Option<String> {
        assert!(
            needs.len() <= 1 && needs.iter().all(|need| need.kind == NeedKind::Sdk),
            "at most one SDK need: {needs:?}"
        );
        needs.into_iter().next().map(|need| need.text)
    }

    fn sdk(text: &str) -> Need {
        Need {
            kind: NeedKind::Sdk,
            text: text.to_owned(),
        }
    }

    /// The rollForward table of <https://learn.microsoft.com/dotnet/core/tools/global-json>, ported from the PowerShell
    /// client's: one version each policy accepts and one it refuses; x.y.znn, z the feature band and nn the patch.
    /// Each row: the requested version, the policy (empty for the default), whether prerelease SDKs are allowed, the
    /// SDKs installed, and the one chosen (`None`, no SDK).
    #[test]
    fn each_roll_forward_policy_chooses_as_the_global_json_page_says() {
        /// The requested version, the policy, whether prereleases are allowed, the SDKs installed, the one chosen.
        type Row = (&'static str, &'static str, bool, &'static [&'static str], Option<&'static str>);
        let rows: [Row; 28] = [
            ("10.0.401", "", true, &["10.0.400", "10.0.403"], Some("10.0.403")),
            ("10.0.401", "", true, &["10.0.400", "10.0.500"], None),
            ("10.0.401", "patch", true, &["10.0.401", "10.0.403"], Some("10.0.401")),
            ("10.0.401", "patch", true, &["10.0.400", "10.0.500"], None),
            (
                "10.0.401",
                "feature",
                true,
                &["10.0.400", "10.0.500", "10.0.502", "10.0.600"],
                Some("10.0.502"),
            ),
            ("10.0.401", "feature", true, &["10.0.400", "10.1.100"], None),
            ("10.0.401", "minor", true, &["10.1.100", "10.1.103", "10.2.100"], Some("10.1.103")),
            ("10.0.401", "minor", true, &["10.0.300", "11.0.100"], None),
            ("10.0.401", "major", true, &["11.0.100", "11.0.102", "12.0.100"], Some("11.0.102")),
            ("10.0.401", "major", true, &["9.0.500", "10.0.400"], None),
            ("10.0.401", "latestPatch", true, &["10.0.401", "10.0.499", "10.0.500"], Some("10.0.499")),
            ("10.0.401", "latestPatch", true, &["10.0.400", "10.0.500"], None),
            ("10.0.401", "latestFeature", true, &["10.0.401", "10.0.600", "10.1.100"], Some("10.0.600")),
            ("10.0.401", "latestFeature", true, &["10.0.400", "10.1.100"], None),
            ("10.0.401", "latestMinor", true, &["10.0.401", "10.2.100", "11.0.100"], Some("10.2.100")),
            ("10.0.401", "latestMinor", true, &["10.0.400", "11.0.100"], None),
            ("10.0.401", "latestMajor", true, &["10.0.401", "12.0.100"], Some("12.0.100")),
            ("10.0.401", "latestMajor", true, &["9.0.500", "10.0.400"], None),
            ("10.0.401", "disable", true, &["10.0.401", "10.0.402"], Some("10.0.401")),
            ("10.0.401", "disable", true, &["10.0.400", "10.0.402"], None),
            ("10.0.401", "latestFeature", true, &["10.0.401", "8.0.425"], Some("10.0.401")),
            ("10.0.401", "LatestFeature", true, &["10.0.402"], Some("10.0.402")),
            ("10.0.401", "sideways", true, &["10.0.401"], None),
            ("10.0", "latestFeature", true, &["10.0.401"], None),
            (
                "10.0.100",
                "latestFeature",
                true,
                &["10.0.100", "10.0.200-preview.1.2"],
                Some("10.0.200-preview.1.2"),
            ),
            (
                "10.0.100",
                "latestFeature",
                false,
                &["10.0.100", "10.0.200-preview.1.2"],
                Some("10.0.100"),
            ),
            ("10.0.100-rc.1.1", "latestPatch", true, &["10.0.100-rc.1.1", "10.0.100"], Some("10.0.100")),
            (
                "10.0.100-rc.1.1",
                "latestPatch",
                true,
                &["10.0.100-rc.2.1", "10.0.100-rc.10.1"],
                Some("10.0.100-rc.10.1"),
            ),
        ];
        for (requested, policy, prerelease, available, want) in rows {
            let available = owned(available);
            assert_eq!(
                resolve(requested, policy, prerelease, &available),
                want,
                "{requested} rollForward '{policy}' allowPrerelease {prerelease} against {available:?}"
            );
        }
    }

    #[test]
    fn a_prerelease_pin_is_refused_when_prereleases_are_not_allowed() {
        let available = owned(&["10.0.100-rc.1.1"]);
        assert_eq!(resolve("10.0.100-rc.1.1", "disable", true, &available), Some("10.0.100-rc.1.1"));
        assert_eq!(resolve("10.0.100-rc.1.1", "disable", false, &available), None);
    }

    #[test]
    fn a_version_s_third_number_is_its_feature_band_and_patch() {
        let version = SdkVersion::parse("10.0.401").expect("a version");
        assert_eq!(
            (version.major, version.minor, version.band, version.patch, version.pre),
            (10, 0, 4, 1, "")
        );
        let version = SdkVersion::parse("9.0.1234-preview.7.25").expect("a version with a label");
        assert_eq!((version.band, version.patch, version.pre), (12, 34, "preview.7.25"));
        let version = SdkVersion::parse("8.0.99").expect("a version below the first band");
        assert_eq!((version.band, version.patch), (0, 99));
        for text in [
            "10.0",
            "10.0.401.1",
            "10.0.x",
            "v10.0.401",
            "10.0.401-",
            "10.0.401-rc_1",
            "",
            " 10.0.401",
            "10..401",
        ] {
            assert!(SdkVersion::parse(text).is_none(), "'{text}' is no version");
        }
    }

    #[test]
    fn versions_order_by_their_parts_then_their_prerelease_labels() {
        let order = |left: &str, right: &str| {
            compare(
                &SdkVersion::parse(left).expect("a version"),
                &SdkVersion::parse(right).expect("a version"),
            )
        };
        assert_eq!(order("10.0.401", "10.0.400"), Ordering::Greater);
        assert_eq!(order("10.0.401", "10.0.0401"), Ordering::Equal, "leading zeros are the same number");
        assert_eq!(order("10.0.100", "10.0.100-rc.1"), Ordering::Greater, "a release above its prerelease");
        assert_eq!(order("10.0.100-rc.2", "10.0.100-rc.10"), Ordering::Less, "numbers by value");
        assert_eq!(order("10.0.100-rc.1", "10.0.100-rc.a"), Ordering::Less, "numbers below words");
        assert_eq!(order("10.0.100-alpha", "10.0.100-beta"), Ordering::Less, "words ordinally");
        assert_eq!(
            order("10.0.100-rc", "10.0.100-rc.1"),
            Ordering::Less,
            "a label that runs out first is lower"
        );
        assert_eq!(order("10.0.100-rc.01", "10.0.100-rc.1"), Ordering::Equal);
    }

    /// The pin read from a `global.json`, ported from the PowerShell client's table: one with a pin, one without
    /// `sdk.rollForward`, one without `sdk`, one malformed, one whose comments are stripped, and none at all.
    #[test]
    fn the_pin_is_read_from_the_global_json() {
        let pin = |found, version: Option<&str>, roll_forward: Option<&str>, allow_prerelease| Pin {
            found,
            version: version.map(str::to_owned),
            roll_forward: roll_forward.map(str::to_owned),
            allow_prerelease,
        };
        let rows = [
            (
                Some(r#"{ "sdk": { "version": "10.0.401", "rollForward": "latestFeature", "allowPrerelease": false } }"#),
                pin(true, Some("10.0.401"), Some("latestFeature"), false),
            ),
            (Some(r#"{ "sdk": { "version": "8.0.425" } }"#), pin(true, Some("8.0.425"), None, true)),
            (
                Some(r#"{ "test": { "runner": "Microsoft.Testing.Platform" } }"#),
                pin(true, None, None, true),
            ),
            (Some(r#"{ "sdk": { "version": "10.0.401""#), pin(true, None, None, true)),
            (None, pin(false, None, None, true)),
            (Some(r#"{ "sdk": { "version": 10 } }"#), pin(true, None, None, true)),
            (Some(r#"{ "sdk": { "version": "" } }"#), pin(true, None, None, true)),
            (Some(r#"["sdk"]"#), pin(true, None, None, true)),
            // The members are spelled exactly as dotnet spells them, so another letter case is no pin, and a byte order
            // mark is skipped before one.
            (
                Some("\u{feff}{ \"SDK\": { \"Version\": \"9.0.100\", \"rollForward\": 1, \"allowPrerelease\": 0 } }"),
                pin(true, None, None, true),
            ),
            (
                Some("\u{feff}{ \"sdk\": { \"version\": \"9.0.100\" } }"),
                pin(true, Some("9.0.100"), None, true),
            ),
            // The version is the three-part `x.y.z` dotnet reads; anything else is no pin at all.
            (
                Some(r#"{ "sdk": { "version": "10.0.100-preview.7.25380.108" } }"#),
                pin(true, Some("10.0.100-preview.7.25380.108"), None, true),
            ),
            (Some(r#"{ "sdk": { "version": "999.0.0" } }"#), pin(true, Some("999.0.0"), None, true)),
            (Some(r#"{ "sdk": { "version": "8.0" } }"#), pin(true, None, None, true)),
            (Some(r#"{ "sdk": { "version": "8.0.100.1" } }"#), pin(true, None, None, true)),
            (Some(r#"{ "sdk": { "version": "v8.0.100" } }"#), pin(true, None, None, true)),
            (Some(r#"{ "sdk": { "version": " 8.0.100" } }"#), pin(true, None, None, true)),
            (Some(r#"{ "sdk": { "version": "8.0.100 " } }"#), pin(true, None, None, true)),
            (Some(r#"{ "sdk": { "version": "8.0.x" } }"#), pin(true, None, None, true)),
            // A rollForward names one of dotnet's policies, its letter case aside, or the pin is no pin at all.
            (
                Some(r#"{ "sdk": { "version": "10.0.401", "rollForward": "LATESTFEATURE" } }"#),
                pin(true, Some("10.0.401"), Some("LATESTFEATURE"), true),
            ),
            (
                Some(r#"{ "sdk": { "version": "10.0.401", "rollForward": "sideways" } }"#),
                pin(true, None, None, true),
            ),
            (
                Some(r#"{ "sdk": { "version": "10.0.401", "rollForward": 1 } }"#),
                pin(true, None, None, true),
            ),
            // allowPrerelease is a JSON boolean: absent is true, and no other type is a pin.
            (
                Some(r#"{ "sdk": { "version": "10.0.401", "allowPrerelease": true } }"#),
                pin(true, Some("10.0.401"), None, true),
            ),
            (
                Some(r#"{ "sdk": { "version": "10.0.401", "allowPrerelease": 0 } }"#),
                pin(true, None, None, true),
            ),
            (
                Some(r#"{ "sdk": { "version": "10.0.401", "allowPrerelease": "false" } }"#),
                pin(true, None, None, true),
            ),
            // Comments are stripped as dotnet strips them: outside the strings, and an unterminated one drops the rest.
            (
                Some("// the sdk\n{ \"sdk\": { \"version\": \"10.0.401\" } }"),
                pin(true, Some("10.0.401"), None, true),
            ),
            (
                Some("{ /* the sdk */ \"sdk\": { \"version\": \"10.0.401\" } }"),
                pin(true, Some("10.0.401"), None, true),
            ),
            (
                Some("{ \"sdk\": { \"version\": \"10.0.401\" } } // trailing"),
                pin(true, Some("10.0.401"), None, true),
            ),
            (
                Some(r#"{ "sdk": { "version": "10.0.401", "rollForward": "patch", "x": "a//b/*c" } }"#),
                pin(true, Some("10.0.401"), Some("patch"), true),
            ),
            (Some(r#"{ "sdk": /* "version": "10.0.401" } }"#), pin(true, None, None, true)),
            // A removed block comment is one space, so `1/**/2` does not join into `12`.
            (Some(r#"{ "sdk": { "version": "999.0.0"/**/ } }"#), pin(true, Some("999.0.0"), None, true)),
            (Some(r#"{"sdk":{"version":"9.0.100","x":1/**/2}}"#), pin(true, None, None, true)),
            // dotnet reads the first JSON value and ignores any text after it.
            (
                Some("{ \"sdk\": { \"version\": \"10.0.401\" } }\n}\n"),
                pin(true, Some("10.0.401"), None, true),
            ),
            (
                Some("{ \"sdk\": { \"version\": \"10.0.401\" } }\ngarbage text\n"),
                pin(true, Some("10.0.401"), None, true),
            ),
            (
                Some(r#"{ "sdk": { "version": "10.0.401" } } { "sdk": { "version": "8.0.425" } }"#),
                pin(true, Some("10.0.401"), None, true),
            ),
        ];
        for (text, want) in rows {
            assert_eq!(pin_of(text), want, "{text:?}");
        }
    }

    #[test]
    fn a_pin_needs_the_sdk_it_resolves_to_and_never_reads_the_pc_s_sdks() {
        let pinned = pin_of(Some(r#"{ "sdk": { "version": "999.0.0" } }"#));
        let read = Cell::new(false);
        let local = || {
            read.set(true);
            10
        };
        assert_eq!(
            the_sdk_need(need(&pinned, &owned(&["10.0.100"]), local)).as_deref(),
            Some(".NET SDK 999.0.0 (rollForward patch)"),
            "the policy defaults to patch"
        );
        assert!(!read.get(), "a pin decides alone");
        let served = pin_of(Some(r#"{ "sdk": { "version": "10.0.100", "rollForward": "latestMajor" } }"#));
        assert_eq!(the_sdk_need(need(&served, &owned(&["10.0.100", "11.0.100"]), || 10)), None);
        let disabled = pin_of(Some(r#"{ "sdk": { "version": "99.0.100", "rollForward": "disable" } }"#));
        assert_eq!(
            the_sdk_need(need(&disabled, &[], || 10)).as_deref(),
            Some(".NET SDK 99.0.100 (rollForward disable)")
        );
    }

    #[test]
    fn a_tree_with_no_pin_needs_an_sdk_of_the_pc_s_newest_major() {
        let unpinned = pin_of(None);
        assert_eq!(
            the_sdk_need(need(&unpinned, &owned(&["8.0.425", "10.0.100"]), || 10)),
            None,
            "the major is there"
        );
        assert_eq!(
            the_sdk_need(need(&unpinned, &owned(&["8.0.425"]), || 10)).as_deref(),
            Some("a .NET 10 SDK")
        );
        assert_eq!(
            the_sdk_need(need(&unpinned, &[], || 10)).as_deref(),
            Some("a .NET 10 SDK"),
            "a builder with none"
        );
        assert_eq!(
            the_sdk_need(need(&unpinned, &owned(&["8.0.425"]), || 0)),
            None,
            "a PC with no SDK checks nothing"
        );
        let unreadable = pin_of(Some("{ not json"));
        assert_eq!(
            the_sdk_need(need(&unreadable, &owned(&["8.0.425"]), || 999)).as_deref(),
            Some("a .NET 999 SDK")
        );
    }

    #[test]
    fn the_pc_s_newest_major_is_read_from_its_sdk_list() {
        assert_eq!(newest_major(["8.0.425", "10.0.100-rc.1", "9.0.300"]), 10);
        assert_eq!(newest_major(["not-a-version"]), 0);
        assert_eq!(newest_major([]), 0);
    }

    #[test]
    fn the_mismatch_line_names_the_repo_its_need_the_host_and_its_sdks() {
        assert_eq!(
            mismatch_line(
                "app",
                &sdk(".NET SDK 999.0.0 (rollForward patch)"),
                "builder",
                &have(NeedKind::Sdk, &owned(&["8.0.425", "10.0.100"]))
            ),
            "gate: remote toolchain mismatch: app needs .NET SDK 999.0.0 (rollForward patch); builder has 8.0.425, 10.0.100"
        );
        assert_eq!(
            mismatch_line("app", &sdk("a .NET 10 SDK"), "builder", &have(NeedKind::Sdk, &listed("none"))),
            "gate: remote toolchain mismatch: app needs a .NET 10 SDK; builder has none"
        );
    }

    #[test]
    fn each_need_says_its_own_mismatch_line_in_the_order_of_the_needs() {
        let needs = [sdk("a .NET 10 SDK"), sdk(".NET SDK 999.0.0 (rollForward patch)")];
        assert_eq!(
            lines("app", &needs, "builder", &owned(&["8.0.425"])),
            [
                "gate: remote toolchain mismatch: app needs a .NET 10 SDK; builder has 8.0.425",
                "gate: remote toolchain mismatch: app needs .NET SDK 999.0.0 (rollForward patch); builder has 8.0.425",
            ]
        );
        assert_eq!(lines("app", &[], "builder", &[]), Vec::<String>::new(), "no need says no line");
    }

    #[test]
    fn an_sdk_list_drops_none_and_empty_entries() {
        assert_eq!(listed("8.0.425, 10.0.100"), owned(&["8.0.425", "10.0.100"]));
        assert_eq!(listed("none"), Vec::<String>::new());
        assert_eq!(listed("NONE,"), Vec::<String>::new());
        assert_eq!(listed(" 9.0.100 ,,none"), owned(&["9.0.100"]));
    }

    /// Ported from the PowerShell client's no-pin scope table, with the global.json and letter-case shapes.
    #[test]
    fn a_dotnet_tree_holds_a_global_json_or_a_solution_or_a_project_anywhere() {
        for (names, want) in [
            (&[][..], false),
            (&["README.md", "src/main.rs"][..], false),
            (&["src/App/App.csproj"][..], true),
            (&["tools/Tools.slnx"][..], true),
            (&["Old.sln"][..], true),
            (&["global.json"][..], true),
            (&["tools/global.json"][..], true),
            (&["SRC/APP.CSPROJ"][..], true),
            (&["myglobal.json", "App.csproj.user", "notes.sln.md"][..], false),
        ] {
            assert_eq!(is_dotnet_tree(names.iter().copied()), want, "{names:?}");
        }
    }
}
