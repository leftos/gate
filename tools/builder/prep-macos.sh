#!/usr/bin/env bash
# Prepares a Mac to be a gate builder, one step script per item of a checklist: the builder account, Remote Login, the
# PC's ssh key, optional Tailscale, ssh for the builder only, the Xcode license, FileVault off and automatic login. The
# steps, their defaults and the caveat of skipping each are in prep/macos/steps.tsv; each step script stands alone and
# is idempotent.
#
# Run it as an admin account under sudo, at the console or over ssh -t:
#   sudo bash tools/builder/prep-macos.sh --user <builder user> --public-key-file <path to the PC's public key>
#
# Flags:
#   --user <name>             the builder account; required except with --list.
#   --public-key-file <path>  the PC's public key, which the authorized-key step needs.
#   --list                    prints the steps, their defaults, flags and caveats, and exits 0.
#   --defaults                runs the default steps not yet done, without the menu; each done one logs
#                             SKIP <id>: done at <time>.
#   --steps <id>[,<id>...]    runs the named steps, without the menu; not with --defaults.
#   --dry-run                 each step says what it would change and changes nothing; needs no root, and writes no
#                             log or state.
#   --yes                     does not ask. Otherwise --defaults and --steps ask "prep: run <ids>? [y/N]" at a
#                             console (a no exits 1: prep: not confirmed; nothing ran) and run unasked without one.
# With neither --defaults nor --steps it shows a numbered menu: numbers toggle steps, a ticks all, d resets to the
# defaults, Enter runs, q quits. Steps already done (the state file) show as [done] and are not ticked again. With
# nothing ticked it prints "prep: nothing to run" and exits 0.
#
# Every step logs BEGIN <id>, then OK <id>, SKIP <id>: <why> or FAIL <id>: exit <n>, to stdout and timestamped to
# /Library/Application Support/gate-prep/prep.log, and the run ends with COMPLETE failed=<n>; a step's own output goes
# to the console only. A step that ends OK is added to /Library/Application Support/gate-prep/state as one
# "<id><tab><time>" line. Exits: 0 when no step failed, 1 when one did, the run lacks root or the question was
# declined, 2 on a bad argument.
#
# Test hooks: GATE_PREP_ROOT, when set, is put in front of every system path (the state, the log and the steps' /etc and
# /Library files); GATE_PREP_TEST_UID stands in for `id -u`; GATE_PREP_TEST_TTY=1 takes stdin as a console.
set -euo pipefail

prog='prep'
here=$(cd "$(dirname "$0")" && pwd)
steps_dir="$here/prep/macos"
prep_dir="${GATE_PREP_ROOT:-}/Library/Application Support/gate-prep"
state_file="$prep_dir/state"
log_file="$prep_dir/prep.log"

die() {
    printf '%s: %s\n' "$prog" "$1" >&2
    exit "${2:-2}"
}

user=''
key_file=''
list=0
defaults=0
steps_arg=''
dry_run=0
yes=0
usage_flags='--user <name>, --public-key-file <path>, --list, --defaults, --steps <id>[,<id>...], --dry-run, --yes'
while [ "$#" -gt 0 ]; do
    case "$1" in
        --user | --public-key-file | --steps)
            [ "$#" -ge 2 ] && [ -n "$2" ] || die "$1 needs a value"
            case "$1" in
                --user) user="$2" ;;
                --public-key-file) key_file="$2" ;;
                --steps) steps_arg="$2" ;;
            esac
            shift 2
            ;;
        --list) list=1 && shift ;;
        --defaults) defaults=1 && shift ;;
        --dry-run) dry_run=1 && shift ;;
        --yes) yes=1 && shift ;;
        *) die "unknown argument '$1'. Flags: $usage_flags" ;;
    esac
done

ids=()
names=()
defs=()
destructive=()
interactive=()
caveats=()
load_manifest() {
    local manifest="$steps_dir/steps.tsv" n=0 id name def destr inter caveat detail extra
    [ -f "$manifest" ] || die "no manifest at $manifest; run prep-macos.sh from its own folder in the gate checkout" 1
    while IFS=$'\t' read -r id name def destr inter caveat detail extra; do
        n=$((n + 1))
        [ "$n" -gt 1 ] || continue
        [ -n "$detail" ] && [ -z "$extra" ] || die "$manifest line $n does not have 7 tab-separated fields" 1
        [ -f "$steps_dir/$id.sh" ] || die "$manifest names the step '$id', which has no $steps_dir/$id.sh" 1
        ids+=("$id")
        names+=("$name")
        defs+=("$def")
        destructive+=("$destr")
        interactive+=("$inter")
        caveats+=("$caveat")
    done <"$manifest"
}
load_manifest
count=${#ids[@]}

print_list() {
    local i flags
    printf '%3s  %-16s %-30s %-8s %-12s %s\n' '#' id step default flags 'caveat if skipped'
    for ((i = 0; i < count; i++)); do
        flags=''
        [ "${interactive[$i]}" = 1 ] && flags='interactive'
        [ "${destructive[$i]}" = 1 ] && flags="${flags:+$flags,}destructive"
        printf '%3s  %-16s %-30s %-8s %-12s %s\n' "$((i + 1))" "${ids[$i]}" "${names[$i]}" \
            "$([ "${defs[$i]}" = 1 ] && echo yes || echo no)" "${flags:--}" "${caveats[$i]}"
    done
}
if [ "$list" = 1 ]; then
    print_list
    exit 0
fi

[ -n "$user" ] || die "--user <name> is required. Flags: $usage_flags"
[[ "$user" =~ ^[A-Za-z_][A-Za-z0-9_.-]*$ ]] ||
    die "--user '$user' is not an account name: a letter or _, then letters, digits, _, . or -"
[ "$defaults" = 0 ] || [ -z "$steps_arg" ] || die '--defaults and --steps are separate runs; pass one'

# The state file holds one "<id><tab><time>" line per step that ended OK.
done_ids=''
[ ! -f "$state_file" ] || done_ids=$(cut -f 1 "$state_file")
is_done() { printf '%s\n' "$done_ids" | grep -qxF -- "$1"; }
done_time() { awk -F '\t' -v id="$1" '$1 == id { print $2; exit }' "$state_file"; }

# sel[i] is 1 when step i is ticked.
sel=()
select_defaults() {
    local i
    for ((i = 0; i < count; i++)); do
        if [ "${defs[$i]}" = 1 ] && ! is_done "${ids[$i]}"; then sel[i]=1; else sel[i]=0; fi
    done
}
select_named() {
    local want i found joined
    for ((i = 0; i < count; i++)); do sel[i]=0; done
    while IFS= read -r want; do
        [ -n "$want" ] || continue
        found=0
        for ((i = 0; i < count; i++)); do
            if [ "${ids[$i]}" = "$want" ]; then sel[i]=1 && found=1; fi
        done
        if [ "$found" = 0 ]; then
            joined=$(printf '%s, ' "${ids[@]}")
            die "--steps '$want' is not a step. Steps: ${joined%, }"
        fi
    done < <(printf '%s\n' "$steps_arg" | tr ',' '\n')
}

# Refuses, before anything runs, a ticked step that lacks the flag it needs, or one marked destructive.
check_step_flags() {
    local i
    for ((i = 0; i < count; i++)); do
        [ "${sel[$i]}" = 1 ] || continue
        [ "${destructive[$i]}" = 0 ] || die "step '${ids[$i]}' is marked destructive, and prep-macos.sh has no confirmation for one" 1
        [ "${ids[$i]}" = authorized-key ] || continue
        [ -n "$key_file" ] || die 'authorized-key needs --public-key-file <path>, the PC'"'"'s public key'
        [ -f "$key_file" ] || die "--public-key-file '$key_file' is not a file"
    done
}
selected_ids() {
    local i out=''
    for ((i = 0; i < count; i++)); do [ "${sel[$i]}" = 0 ] || out="${out:+$out }${ids[$i]}"; done
    printf '%s' "$out"
}

has_console() { [ -t 0 ] || [ "${GATE_PREP_TEST_TTY:-}" = 1 ]; }

draw_menu() {
    local i box root_word=no
    [ "${GATE_PREP_TEST_UID:-$(id -u)}" != 0 ] || root_word=yes
    printf 'gate builder prep  (root: %s, user: %s%s)\n\n' "$root_word" "$user" "$([ "$dry_run" = 1 ] && echo ', dry run')"
    printf '  #  [x] %-30s %s\n' step 'caveat if skipped'
    for ((i = 0; i < count; i++)); do
        if [ "${sel[$i]}" = 1 ]; then
            box='[x]'
        elif is_done "${ids[$i]}"; then
            printf '%3s  [done] %s\n' "$((i + 1))" "${names[$i]}"
            continue
        else
            box='[ ]'
        fi
        printf '%3s  %s %-30s %s\n' "$((i + 1))" "$box" "${names[$i]}" "${caveats[$i]}"
    done
    printf 'toggle: numbers (e.g. 4 7), a=all, d=defaults, Enter=run, q=quit\n> '
}

# Applies one line of menu input: prints "run", "quit" or "again"; a line holding any bad token changes nothing.
apply_menu_line() {
    local line="$1" token bad=0 n i
    local -a tokens next=("${sel[@]}")
    read -ra tokens <<<"${line//,/ }"
    [ "${#tokens[@]}" -gt 0 ] || { echo run && return; }
    for token in "${tokens[@]}"; do
        case "$token" in
            q | Q) echo quit && return ;;
            a | A) for ((i = 0; i < count; i++)); do is_done "${ids[$i]}" || next[i]=1; done ;;
            d | D)
                for ((i = 0; i < count; i++)); do
                    if [ "${defs[$i]}" = 1 ] && ! is_done "${ids[$i]}"; then next[i]=1; else next[i]=0; fi
                done
                ;;
            *[!0-9]* | '') bad=1 && printf "%s: '%s' is not a step number, a, d or q\n" "$prog" "$token" >&2 ;;
            *)
                n=$((10#$token))
                if [ "$n" -ge 1 ] && [ "$n" -le "$count" ]; then
                    next[n - 1]=$((1 - next[n - 1]))
                else
                    bad=1 && printf "%s: '%s' is not a step number, a, d or q\n" "$prog" "$token" >&2
                fi
                ;;
        esac
    done
    [ "$bad" = 1 ] || sel=("${next[@]}")
    echo again
}

run_menu() {
    local line outcome
    has_console || die 'no console for the menu; run it at the console, over ssh -t, or with --defaults or --steps'
    select_defaults
    while :; do
        draw_menu
        if ! IFS= read -r line; then
            printf '\n'
            exit 0
        fi
        apply_menu_line "$line" >"$scratch_reply" 2>&1 || true
        outcome=$(tail -n 1 "$scratch_reply")
        sed '$d' "$scratch_reply"
        case "$outcome" in
            run) return 0 ;;
            quit) exit 0 ;;
        esac
    done
}

# Asks to go on at a console, unless --yes; without a console the run goes on unasked.
confirm_run() {
    local answer
    [ "$yes" = 0 ] && has_console || return 0
    printf '%s: run %s? [y/N] ' "$prog" "$(selected_ids)"
    IFS= read -r answer || answer=''
    case "$answer" in
        y | Y | yes) ;;
        *) die 'not confirmed; nothing ran' 1 ;;
    esac
}

log() {
    local line
    line="$(date '+%Y-%m-%dT%H:%M:%S') $1"
    printf '%s\n' "$line"
    if [ "$dry_run" = 0 ]; then printf '%s\n' "$line" >>"$log_file"; fi
}

# Logs SKIP for each default step --defaults leaves out because the state file has it.
log_done_defaults() {
    local i
    [ "$defaults" = 1 ] || return 0
    for ((i = 0; i < count; i++)); do
        if [ "${defs[$i]}" = 1 ] && is_done "${ids[$i]}"; then log "SKIP ${ids[$i]}: done at $(done_time "${ids[$i]}")"; fi
    done
}

# Runs one step with its output on the console only; the log gets its outcome, and a SKIP line's reason.
failed=0
run_one() {
    local id="$1" rc why
    local -a words=(--user "$user")
    [ "$id" != authorized-key ] || words+=(--public-key-file "$key_file")
    [ "$dry_run" = 0 ] || words+=(--dry-run)
    log "BEGIN $id"
    set +e
    bash "$steps_dir/$id.sh" "${words[@]}" 2>&1 | tee "$scratch_out"
    rc=${PIPESTATUS[0]}
    set -e
    if [ "$rc" != 0 ]; then
        log "FAIL $id: exit $rc"
        failed=$((failed + 1))
    elif why=$(grep -m 1 '^SKIP ' "$scratch_out"); then
        log "SKIP $id: ${why#SKIP }"
    else
        log "OK $id"
        if [ "$dry_run" = 0 ] && ! is_done "$id"; then
            printf '%s\t%s\n' "$id" "$(date '+%Y-%m-%dT%H:%M:%S')" >>"$state_file"
            done_ids="$done_ids"$'\n'"$id"
        fi
    fi
}

menu_run=0
if [ "$defaults" = 1 ]; then
    select_defaults
elif [ -n "$steps_arg" ]; then
    select_named
else
    menu_run=1
fi
[ "$menu_run" = 1 ] || check_step_flags
if [ "$dry_run" = 0 ] && [ "${GATE_PREP_TEST_UID:-$(id -u)}" != 0 ]; then
    die "run it with sudo: sudo bash $0 --user $user ..." 1
fi

scratch_out=$(mktemp "${TMPDIR:-/tmp}/prep-macos.XXXXXX")
scratch_reply=$(mktemp "${TMPDIR:-/tmp}/prep-macos.XXXXXX")
trap 'rm -f "$scratch_out" "$scratch_reply"' EXIT
if [ "$menu_run" = 1 ]; then
    run_menu
    check_step_flags
elif [ -n "$(selected_ids)" ]; then
    confirm_run
fi

if [ "$dry_run" = 0 ]; then mkdir -p "$prep_dir"; fi
log_done_defaults
if [ -z "$(selected_ids)" ]; then
    printf '%s: nothing to run\n' "$prog"
    exit 0
fi
for ((i = 0; i < count; i++)); do
    [ "${sel[$i]}" = 0 ] || run_one "${ids[$i]}"
done
log "COMPLETE failed=$failed"
[ "$failed" = 0 ]
