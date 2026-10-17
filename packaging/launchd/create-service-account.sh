#!/bin/sh
# MAN-268: create the disabled-login _manta user and group that both manta
# LaunchDaemons run as. Run once, before installing either plist:
#   sudo sh packaging/launchd/create-service-account.sh
# Re-running accepts a matching account. Any other existing _manta state,
# no free ID or a failed command stops with an error; nothing is deleted
# or overwritten. Install steps: packaging/README.md.
set -eu

name=_manta
shell=/usr/bin/false
home=/var/empty
first_id=400
last_id=499

fail() {
    printf 'create-service-account: %s\n' "$*" >&2
    exit 1
}

for tool in dscl dscacheutil id; do
    command -v "$tool" >/dev/null 2>&1 || fail "$tool not found; this helper is for macOS"
done
euid=$(id -u) || fail "cannot read the effective user ID"
[ "$euid" = 0 ] || fail "must run as root: sudo sh $0"

# `dscl . -list` output is "<name> <value>..." per record. "." is the local
# node, /Local/Default.
has_record() {
    printf '%s\n' "$1" | awk -v n="$2" '$1 == n { f = 1 } END { exit !f }'
}
has_id() {
    printf '%s\n' "$1" | awk -v n="$2" '{ for (i = 2; i <= NF; i++) if ($i == n) f = 1 } END { exit !f }'
}

# One attribute's value from `dscl . -read`, label removed; dscl prints
# either "Key: value" or "Key:" with the value on the next line.
read_attr() {
    out=$(dscl . -read "$1" "$2") || return 1
    printf '%s\n' "$out" | tr '\n' ' ' | sed "s/^$2:[[:space:]]*//; s/[[:space:]]*\$//"
}

# Directory resolution goes through dscacheutil, which answers from every
# directory node in the search policy (local and network). It prints
# nothing when no record matches.
resolves() {
    found=$(dscacheutil -q "$1" -a "$2" "$3") || return 2
    [ -n "$found" ]
}

# Lowest ID in first_id..last_id that is neither in the local list nor
# resolvable through the directory. Status 1: none free; 2: lookup failed.
free_id() {
    n=$first_id
    while [ "$n" -le "$last_id" ]; do
        if ! has_id "$1" "$n"; then
            rc=0
            resolves "$2" "$3" "$n" || rc=$?
            case $rc in
                0) ;;
                1) printf '%s\n' "$n"; return 0 ;;
                *) return 2 ;;
            esac
        fi
        n=$((n + 1))
    done
    return 1
}

verify() {
    got_uid=$(id -u "$name") || fail "id $name does not resolve"
    got_gid=$(id -g "$name") || fail "id $name does not resolve"
    if [ "$got_uid" != "$1" ] || [ "$got_gid" != "$2" ]; then
        fail "id $name reports uid $got_uid gid $got_gid, expected uid $1 gid $2"
    fi
    printf 'manta service account ready: %s\n' "$name"
    exit 0
}

users=$(dscl . -list /Users UniqueID) || fail "dscl . -list /Users UniqueID failed"
groups=$(dscl . -list /Groups PrimaryGroupID) || fail "dscl . -list /Groups PrimaryGroupID failed"

user_exists=no
group_exists=no
if has_record "$users" "$name"; then user_exists=yes; fi
if has_record "$groups" "$name"; then group_exists=yes; fi

if [ "$user_exists" = yes ] && [ "$group_exists" = yes ]; then
    gid=$(read_attr "/Groups/$name" PrimaryGroupID) || fail "cannot read PrimaryGroupID of group $name"
    uid=$(read_attr "/Users/$name" UniqueID) || fail "cannot read UniqueID of user $name"
    user_gid=$(read_attr "/Users/$name" PrimaryGroupID) || fail "cannot read PrimaryGroupID of user $name"
    user_shell=$(read_attr "/Users/$name" UserShell) || fail "cannot read UserShell of user $name"
    user_home=$(read_attr "/Users/$name" NFSHomeDirectory) ||
        fail "cannot read NFSHomeDirectory of user $name"
    if [ "$user_shell" != "$shell" ] || [ "$user_home" != "$home" ] || [ "$user_gid" != "$gid" ]; then
        fail "existing user $name has shell $user_shell, home $user_home, group ID $user_gid;" \
            "expected $shell, $home and group $name's ID $gid; left unchanged"
    fi
    verify "$uid" "$gid"
fi
if [ "$user_exists" = yes ]; then
    fail "user $name exists but group $name does not; partial setup left unchanged," \
        "inspect it with dscl . -read /Users/$name"
fi
if [ "$group_exists" = yes ]; then
    fail "group $name exists but user $name does not; partial setup left unchanged," \
        "inspect it with dscl . -read /Groups/$name"
fi

# Not local, but a network directory may still hold the name.
for category in user group; do
    rc=0
    resolves "$category" name "$name" || rc=$?
    case $rc in
        0) fail "a $category named $name resolves through a directory service, not locally; left unchanged" ;;
        1) ;;
        *) fail "dscacheutil -q $category -a name $name failed" ;;
    esac
done

rc=0
gid=$(free_id "$groups" group gid) || rc=$?
case $rc in
    0) ;;
    1) fail "no free group ID in $first_id-$last_id" ;;
    *) fail "dscacheutil group lookup failed" ;;
esac
rc=0
uid=$(free_id "$users" user uid) || rc=$?
case $rc in
    0) ;;
    1) fail "no free user ID in $first_id-$last_id" ;;
    *) fail "dscacheutil user lookup failed" ;;
esac

create() {
    dscl . -create "$@" || fail "dscl . -create $* failed; records created so far are left in place"
}

create "/Groups/$name"
create "/Groups/$name" PrimaryGroupID "$gid"
create "/Groups/$name" RealName "manta CW skimmer"
create "/Groups/$name" Password '*'

create "/Users/$name"
create "/Users/$name" UniqueID "$uid"
create "/Users/$name" PrimaryGroupID "$gid"
create "/Users/$name" UserShell "$shell"
create "/Users/$name" NFSHomeDirectory "$home"
create "/Users/$name" RealName "manta CW skimmer"
create "/Users/$name" Password '*'
create "/Users/$name" IsHidden 1

verify "$uid" "$gid"
