# Architecture

This file describes how pitboard's code is organised and what must stay true in it. It
also holds the measured facts about macOS, Claude Code and OpenAI's Codex CLI that the
design rests on. It changes when the layout changes or a tool build moves a fact, not with
each commit.

## Bird's eye view

pitboard switches Claude Code, Codex or Claude Desktop between a person's own accounts on
one machine. It also shows how much of each account's limits is left. A switch parks the
login in use and puts another account's parked login in its place.

One crate, `pitboard-core`, does this for every tool. It reads and writes each tool's login,
keeps pitboard's index of accounts, and asks each tool's service for usage. Claude Code's and
Codex's logins are credentials, a keychain item or a file. Claude Desktop's is a list of
items in its data folder, moved by rename.

Two front ends use the core: the command line, `pitboard`, on macOS and Linux, and the menu
bar app on macOS 14 or later. The app calls the core through UniFFI bindings. The command
line inside the app is what the app's daily renewal runs, and what the `pitboard-app` cask
puts on `PATH`.

pitboard has no server of its own. The core sends requests only to Anthropic, for Claude
Code, to OpenAI, for Codex, and to claude.ai, for Claude Desktop's live usage once somebody
turns it on.

## Code map

- `crates/pitboard-core`: the engine. Parking, switching, recovery, the stores and usage.
  The front ends reach it through `service::Pitboard`, built with a `context::Context`.
  - `provider/`: one module per tool, `claude`, `codex` and `desktop`, each implementing
    the `Provider` trait in `provider/mod.rs`. The trait covers where the tool keeps its
    login, whose it is, how to renew it and what it has left. Each module's
    `assumptions.rs` is that tool's register of facts. `provider/codex/holders.rs` names
    where a running `codex` can be, and what makes each take a switch.
  - `provider/desktop/`: Claude Desktop. Its login is a `TreeLogin`, not a credential, so
    every credential operation of `Provider` is an error there. `paths.rs` names the items
    a switch moves, `config.rs` the three account keys of `config.json`, `cookies.rs` reads
    the cookie jar with `/usr/bin/sqlite3` without decrypting it, and `identity.rs` says
    whose a data folder is. `holders.rs` is what holds the folder while Claude runs.
    `history.rs` reads the app's own usage history. `safe_storage.rs`, `crypto.rs`,
    `web.rs` and `live_usage.rs` are live usage, the only code that reads Claude's key.
  - `holder.rs`: what keeps a tool's login in memory while it runs, told apart by where
    its program runs from. A switch's warning, `doctor` and the app's offer to quit an app
    all read it, so they cannot disagree.
  - `process.rs`: this user's processes of a program, with where each runs from.
  - `store/`: reading and writing logins. On macOS, parked logins are keychain items. On
    Linux, they are files in the vault. `store/tree.rs` moves a tree login's items, only
    by `rename` on one volume, checked by inode, and deletes nothing but a park.
  - `switch/`: every change to pitboard's index (switching, enrolling, adopting, renaming,
    forgetting, renewing, repairing, abandoning and uninstalling), and the journal that
    finishes an interrupted switch. `switch/tree.rs` is a Claude Desktop switch, enrolment
    and sign-out, and `switch/tree_journal.rs` its own record,
    `~/.pitboard/desktop/journal.json`, which finishes or undoes an interrupted one.
  - `state.rs`: `state.json`, the index of accounts and where each one's login is parked.
  - `lock.rs`: the lock Claude Code takes around credential writes, taken the same way.
  - `context.rs`: what the core takes from its environment, apart from the `PATH` that
    `schedule.rs` reads on Linux.
  - `api.rs`: the requests to Anthropic. The requests to OpenAI are in
    `provider/codex/api.rs`.
  - `status.rs`, `doctor.rs`, `statusline.rs` and `schedule.rs` serve the commands of the
    same names. `schedule.rs` installs daily renewal.
- `crates/pitboard`: the command line. Arguments, rendering for people, the man page, and
  the `--json` contract, pinned by the snapshots in `crates/pitboard/tests/snapshots`.
- `crates/pitboard-ffi`: the core as UniFFI bindings, for the app.
- `crates/uniffi-bindgen-swift`: generates the Swift bindings with exactly the UniFFI
  version the library uses. The bindings check method checksums when they load.
- `crates/pitboard-conformance`: checks a tool's register against a build of that tool.
- `apple/`: the menu bar app. The Swift package holds it as libraries its tests load
  without starting it. `PitboardKit` calls the bindings off the main thread, and
  `PitboardApp` is everything the app does.
  - `Pitboard.xcodeproj` is the app itself. Its `Pitboard` target in `App` starts
    `PitboardApp` and adds Sparkle, and `PitboardUITests` in `UITests` drives it.
  - A renewal schedule written by an app up to 0.3.0 starts the app with `renew`.
    `App/Main.swift` then replaces the process with the command line inside the app.
  - `scripts/build-xcframework.sh` builds the core and its Swift bindings for both Mac
    architectures. `scripts/build-app.sh` builds `Pitboard.app` from them with
    `xcodebuild`, with the command line inside at `Contents/Helpers/pitboard`.
- `packaging/`: the files a release writes into the tap `datlechin/homebrew-tap`. They are
  the casks `pitboard.rb` for the command line and `pitboard-app.rb` for the app,
  `tap_migrations.json`, and the tap's README.
- `docs/`: the Mintlify source of docs.usepitboard.com.
- `website/`: reserved for the usepitboard.com site, to be built with Astro; empty.
- `.github/`:
  - `workflows/ci.yml` checks every push to `main` and every pull request, and
    `workflows/release.yml` turns a `v` tag into a release.
  - `workflows/conformance.yml` checks each tool's newest build against its register.
  - `workflows/sparkle.yml` opens an issue when Sparkle has a release newer than the one
    `Pitboard.xcodeproj` pins, because Dependabot cannot read that pin.
  - `workflows/rotation.yml` rehearses rotating the update key.
  - `actions/apple-keychain` imports the Developer ID certificate for every job that signs.
  - `scripts/` holds the EdDSA key and signature helpers, and the scripts that add Sparkle
    and the command line to the app's bill of materials.
  - `dependabot.yml` asks for weekly updates of Cargo dependencies and GitHub Actions.

## Invariants

- The core prints nothing.
- The core reads its environment in `Context::from_env`, which the command line calls. The
  app builds its own `Context`, because an app opened from Finder has none of the shell's
  environment.
- Two variables are read elsewhere. `PATH` is read when no search path was given
  (`context.rs`), and on Linux to find the program daily renewal runs (`schedule.rs`).
  `XPC_SERVICE_NAME`, which launchd sets, is read in `context.rs`.
- `pitboard-ffi` exports records, enums, one error type, the function `tools` and two
  objects, `SignIn` and `Pitboard`. Every call is synchronous and may block on the
  keychain, a lock or the network. `PitboardKit` makes each call off the main thread.
- On macOS, only `/usr/bin/security` reads or writes Claude Code's keychain item and
  pitboard's parked items. No keychain item is touched through the Security framework. The
  reason is under [macOS](#macos) in Measured facts.
- pitboard takes Claude Code's write lock, the same way Claude Code takes it, before writing
  Claude Code's login. It writes the login where it already lives.
- Codex takes no lock on `auth.json`, so a switch reads the file again before replacing
  it. Every switch reads the live login back rather than trusting its own write.
- pitboard never answers a failed keychain write by writing Claude Code's plaintext file.
  That demotion is Claude Code's to make.
- A Codex login is moved, never copied (`ParkSemantics::MoveOnly`). The parked login is
  read back before the incoming login is written. Codex's own sign-in and sign-out revoke
  the stored refresh token, so two usable copies of one login must never be at rest.
- No login moves until pitboard knows whose it is. A Claude Code login's account is asked of
  Anthropic; a Codex login's is read from its ID token; a Claude Desktop login's is read
  from its files. A session pitboard has not seen, under an account id it knows, moves only
  once the person has enrolled it again, as does a session it knows as one account's under
  another id: E4 measured sign-ins that started with no account id, not one that replaces
  the id Log out leaves behind.
- A Claude Desktop login is a fixed list of files and folders in Claude's data directory,
  and the three keys of its `config.json` that pitboard moves with the account. pitboard moves that list with `rename` and nothing
  else: never a copy, never the whole directory (`TreeLogin::items`).
- Nothing in Claude's data directory moves while anything runs from `Claude.app`, or from
  a bundle under another name whose main program or helper is Claude's, and the check is
  made again before every single move. A process list pitboard cannot read counts as
  Claude running. A `SingletonLock` counts only while its pid still runs Claude's program;
  2.19675.0 keeps none (E14), so the process scan is the check that counts.
- A Claude Desktop park is a directory under `~/.pitboard/desktop/parks`, mode 0700, on
  the same volume as Claude's data directory. A move that would cross volumes is refused,
  because it would be a copy.
- pitboard never deletes what it found in Claude's data directory. What Claude made while
  signed out goes to `~/.pitboard/desktop/strays`. Only `forget` and `uninstall` delete a
  Claude Desktop park, and only inside the parks directory.
- Whose a Claude Desktop login is, is read from files: `lastKnownAccountUuid` and a hash of
  the encrypted `sessionKey`. Switching, enrolling, identifying and verifying never read
  the keychain.
- pitboard reads `Claude Safe Storage` only through `/usr/bin/security`, only after the
  person turned live usage on, and never writes it. Only turning live usage on may show a
  keychain prompt. Every other read gives up within 10 seconds rather than wait on one.
- A Claude Desktop park cannot be renewed. pitboard says when it lapses rather than
  pretend to keep it alive.
- An interrupted Claude Desktop switch has its own record, `~/.pitboard/desktop/journal.json`,
  so it never blocks a Claude Code or Codex command. It is finished only while Claude is
  closed.
- pitboard never renews the login in use. That is the tool's own job, and a second renewer
  would break it.
- Nothing outside `pitboard-core` writes pitboard's index. Every change goes through
  `switch`, which records what it is about to do first and finishes an interrupted change
  before starting another.
- Additive writes become durable before destructive ones. A run that dies midway leaves a
  spare copy of a login, never a missing one.
- The `--json` contract changes only on purpose. A change to a snapshot is a change to the
  contract.
- The state file is read forwards only.

## Boundaries

- The core and its front ends meet at `service::Pitboard`, `context::Context` and the types
  those two return. That is the supported interface of `pitboard-core`. The other public
  modules are public only so the front ends in this repository can reach them, and may
  change in any release.
- In that interface, adding an error, warning or check code is not a breaking change.
  Renaming or removing one is. While pitboard is at 0.x, a breaking change gets a new minor
  version, as in 0.3.0, and after 1.0 a new major version.
- pitboard and each tool meet at the tool's register. What pitboard relies on about a tool
  is written there, and the conformance run checks it against the tool's newest build twice
  a week.
- pitboard and each service meet at a few requests. The list, with what each request
  carries, is in
  [What leaves your machine](https://docs.usepitboard.com/security#what-leaves-your-machine).
- The code and the release meet at a `v` tag, which `.github/workflows/release.yml` turns
  into a release. [RELEASING.md](RELEASING.md) has the procedure.

## The state file

`~/.pitboard/state.json` is pitboard's index: which accounts it knows, and where each one's
login is parked. It carries a `schema` number.

The app and the command line inside it update together. A command line installed another
way updates by its own route. So on one machine, an older pitboard can meet a file a newer
one wrote.

Reading forwards is `state::migrate`. Each schema bump adds an arm that rewrites the
document and falls through to the next. A file two versions behind comes forward in one
read.

Reading backwards is not possible. The older pitboard refuses the file and says to update
it. A bump needs a test that loads a file the previous version wrote.

Schema 4 records each account's tool, and which account is signed in for each tool. A
schema 3 file is brought forward on its first read, with no keychain item or vault file
touched.

A file naming a tool this build does not know is reported as written by a newer pitboard,
not as corrupt. The advice for a corrupt file is to delete it, and following that here would
orphan every parked login.

Claude Desktop added no schema bump. Its accounts are schema 4 entries whose tool is
`desktop` and whose detail is `Detail::Desktop`: the organisation's uuid when known, the
session fingerprint (the SHA-256, in hex, of the `sessionKey` cookie's ciphertext) and when
that session lapses. Its usage is kept under `desktop:<account uuid>`. A pitboard older than
this support reads such a file as naming an unknown tool, `state_names_unknown_tool`.

What a Claude Desktop switch needs beyond the index is in `~/.pitboard/desktop/`, a
directory of mode 0700 whose files are 0600:

- `parks/pitboard-tree-<uuid>-<millis>/`: one parked login, its items, `config-keys.json`
  with the three account keys, and `manifest.json` naming the account, the fingerprint and
  the items.
- `journal.json`: present only while a Claude Desktop switch is unfinished. It names items
  and inodes, never values.
- `strays/<millis>/`: what Claude made while signed out, set aside and never deleted by
  pitboard. One directory per switch or recovery, keeping each item's place; an item that
  would land on another gets a directory of its own.
- `awaiting.json`: the account parked by `pitboard use desktop --signed-out`, until the next
  enrolment.
- `live-usage.json`: whether live usage is on, and whether macOS let pitboard read Claude's
  key. It never holds the key.

## Tool registers

Every fact pitboard relies on about a tool was read out of one build of that tool. Each tool
keeps its facts in a register, `crates/pitboard-core/src/provider/<tool>/assumptions.rs`,
dated with the build they were read from.

A fact names literals a build must contain (`probe`), or literals whose arrival would
disprove it (`absent`). Beside the facts, the register names those that only one system's
build can be read for (`read_on`). A fact about behaviour, such as the 30 second cache,
names no literals. `pitboard-conformance`
tells a macOS build from a Linux one by its header, and reports which facts can still be
read from it, which have moved, which name nothing to look for, and which it skipped as
read from the other system's builds.

The check is shallow on purpose. A literal being present does not prove the behaviour around
it is unchanged. A literal disappearing, or a ruled-out one appearing, does prove something
moved.

On 22 September 2026, the twelve facts the Claude Code register then held were checked
against six macOS builds. They could be read from 2.1.273 to 2.1.278, and the check went
red on 2.1.124. That build predates the credential write lock, two of the five
account-scoped keys and the keychain error classification. On 29 September the register was
read again from the macOS and Linux builds of 2.1.278, 2.1.281 and 2.1.284: every fact
holds on 2.1.281 and 2.1.284, and on 2.1.278 the three that describe the 2.1.281 change are
reported moved, as they should be.

`.github/workflows/conformance.yml` checks the newest build of each tool against its
register on Mondays and Thursdays, or a version given by hand: Claude Code's macOS and
Linux builds, and Codex's Linux build. Its most recent run says
which facts can still be read from the build it checked, and which have moved.

Adding a tool takes three things: a register read out of a named build, a module under
`provider/` implementing `Provider`, and a conformance job. Claude Desktop is the
exception to the last: its literals live in `app.asar`, not in a binary the checker reads,
so every entry has an empty probe and no conformance job runs. Its four entries not yet
measured are dated `UNVERIFIED` and name the experiment that will settle each one, as
[Claude Desktop under Measured facts](#claude-desktop) lists. `ProviderId`,
`ProviderId::ALL` and the matches in `provider::of` and `assumptions::of` name every tool.
The compiler and the tests then point at what an added tool has to fill in.

How to run the checker and add a fact is in
[Tool registers in CONTRIBUTING.md](CONTRIBUTING.md#tool-registers).

## Measured facts

These decide the design. Each gives the build it was read from, or the date it was measured
or written down. The conformance run reads only the facts in a tool's register, and only
by their literals.

### macOS

Measured on macOS 26 and written down on 22 September 2026. Before changing code that
depends on the `security -i` limit or the Security framework's cost, measure them again on
a scratch item.

- `security -i` reads at most 4097 bytes of command from standard input, with no line
  continuation. Its interactive `-w` prompt takes 128 bytes.
- A keychain item written in process, through the Security framework, stays slow to read.
  Every later read of it by `security` takes about a second instead of 0.01 seconds. On a
  scratch item, reads went from 0.01 seconds to 20.55, then settled around 0.8.
- Claude Code reads its login on every cache miss, so writing its item through the framework
  would slow Claude Code for good. pitboard writes a large login on the argument line
  instead, where `ps` can see it for the length of one call.
- Undoing the framework's change needs `security set-key-partition-list`, which asks for the
  keychain password.
- APFS keeps a directory's mtime in nanoseconds, but not exactly: setting one and reading it
  straight back gives a value 18 to 60 nanoseconds away. A lock that remembered the value it
  asked for would abandon every switch, so `lock.rs` keeps the value read back.
- pitboard lists its parked items with `security dump-keychain` without `-d`. It never
  prompts, and emits attributes only, no secret of any item. It exits 0 in 0.06 seconds
  against a keychain of 362 items.
- Reads after a `dump-keychain` take the usual 0.016 seconds, so listing has none of the
  access-list cost of an in-process read. Each service name is on a line of the form
  `"svce"<blob>="<name>"`.

### Claude Code

Read against Claude Code 2.1.284's own storage layer, macOS and Linux builds alike, on 29
September 2026, unless a fact gives its own date. The conformance runs of 24 and 28
September reported three facts moved on 2.1.281 and 2.1.283. All three were false: they
read the Linux build, which has no keychain code, and a `libsecret` that belongs to the Bun
runtime Claude Code ships in. The one real change, in 2.1.281, is how a locked keychain is
treated, and only the macOS build shows it. The run reads both builds since.

- A running session serves its login from a 30 second cache, so it picks up a switch within
  about 33 seconds.
- Every write of the login takes proper-lockfile's directory lock at
  `<storage dir>/.storage-write`: stale after 15000 ms, ten retries, 100 ms to 1000 ms of
  backoff. `lock.rs` carries the same numbers.
- Every write under that lock drops the read cache, reads the login again inside the lock,
  and abandons the write when that read fails. A stale account cannot be written back. From
  2.1.281, a locked keychain counts as a failed read here once the process has seen its
  item; before, it read as empty.
- Claude Code treats its own lock going missing as a warning and keeps writing. pitboard
  cannot expect the other side to stop.
- A write can be marked as already locked without the lock being taken. `/logout` does this
  after retrying for 7.5 seconds, and deletes the login with no lock held.
- The keychain write is `security -i` while the command is at most 4032 bytes. Past that it
  is `add-generic-password -U -a <account> -s <service> -X <hex>`, on the argument line. It
  never refuses or splits a login, and each call has a 2 second timeout.
- A failed write is transient, and does not move the login to the plaintext file, when it
  timed out or, from 2.1.281, when it exited 36 after the process had seen its item. Any
  other failure moves the login to the file.
- The login goes hex-encoded, two characters a byte, so standard input carries about 2 KB of
  it. A larger login is on Claude Code's own argument line at every token refresh. pitboard
  keeps its `security -i` command within the same 4032 bytes, the limit its messages give.
- The keychain read is `find-generic-password -a <account> -w -s <service>`. Exit 0 with
  output is the login; exit 0 with nothing, 44, or output that is not JSON is absent. Exit
  36, a locked keychain, is absent to an ordinary read, a failed read to a write once the
  process has seen its item (from 2.1.281), and a failed read outright only when a caller
  asks. pitboard reads 36 as unreadable on purpose, the strict end of that.
  `security show-keychain-info` exiting 36 only adds an unlock hint.
- On macOS the live chain is the keychain, with the plaintext file `.credentials.json`
  behind it. The successor backend, behind the `tengu_hover_rest` flag, replaces only the
  fallback half, and only for a caller that hands one in. An ordinary `claude` still reads
  the keychain first.
- Claude Code demotes to the plaintext file when a keychain write fails for good, and
  deletes the keychain item when it does. A locked keychain after the item was seen is not
  failing for good, from 2.1.281.
- The supervisor daemon records itself in `<config dir>/daemon.lock`, with its pid and the
  Claude Code version that started it. It leaves the file behind when it stops.
- Claude Code's storage backends are `keychain`, `plaintext` and `windows-credman`, the
  last behind the `tengu_windows_credman` flag. Its code has no `libsecret`,
  `org.freedesktop.secrets`, `gnome-keyring` or `SecretService`. The Linux binary does
  contain `libsecret`, in the Bun runtime it ships in, behind `Bun.secrets`, which Claude
  Code's code never calls.
- `secret-tool` and `kwallet-query` do appear in the bundle, in the credential helpers its
  Bash sandbox keeps out of a shell. So neither name is used to look for a keyring backend.
- On Linux, Claude Code has no keychain backend at all: the plaintext file holds the login.
  Claude Code writes it, then sets its mode to 0600, and pitboard's `PlainUnix` host matches
  that.
- The register holds that absence as `no_keyring_off_macos`, and the conformance run looks
  for `Bun.secrets` and each keyring name in every Linux build. A fact that rests on
  something not existing is wrong the moment it does.
- A Claude Code parked login holds the account's slice of Claude Code's credential
  document. On one real account, measured on 22 September 2026, the slice was 524 bytes
  against 506 for the OAuth block alone. An account holding a device token has not been
  measured.
- Claude Code's config file can be a day behind the login it describes, so pitboard asks
  Anthropic whose a login is. This was written down on 21 September 2026, with no build
  named.
- On 22 September 2026, the machine measured sat within 0.75 seconds of the `Date` header
  of api.anthropic.com across eight requests. `Date` has a granularity of one second.
- That spread is inside the noise, so pitboard keeps no estimate of clock skew. A renewal's
  expiries are counted from the `Date` of the answer that carried them.

### Codex

Read against codex-cli 0.154.0: the binary, its public source at tag `rust-v0.154.0`, and a
real `auth.json` that build wrote. The register is `provider/codex/assumptions.rs`.

- The login is `$CODEX_HOME/auth.json`, by default `~/.codex/auth.json`, at mode 0600.
- `file` is the packaged default store on every platform, and the only one pitboard
  supports. `keyring`, `auto` and `ephemeral` are the others.
- `keyring` and `auto` keep the login in a keychain item, `Codex Auth`, that Codex makes
  through the Security framework. With either of those, the `secret_auth_storage` feature
  keeps it in `secrets/codex_auth.age` instead, under a keychain key.
- Those items trust only `codex`. A read by another program brings up a permission prompt,
  and choosing **Always Allow** would change Codex's item. So pitboard refuses `keyring`
  and `auto`, with or without that feature.
- The login document holds `auth_mode`, `OPENAI_API_KEY`, `last_refresh`, and `tokens` with
  `id_token`, `access_token`, `refresh_token` and `account_id`. `OPENAI_API_KEY` can hold an
  API key Codex obtained at sign-in.
- `codex login` and `codex logout` both POST the stored refresh token to
  `https://auth.openai.com/oauth/revoke` before clearing it. This is why parking a Codex
  login moves it (`ParkSemantics::MoveOnly`).
- A running Codex holds its login in memory for the life of the process and watches no file.
  It refuses a reload whose account id has changed (`Adoption::RestartRequired`), so it
  never picks up a switch.
- A Codex refresh already under way when the file changes writes its own account's tokens
  under whatever account id it finds there.
- Codex writes `auth.json` with no lock of any kind, so there is none for pitboard to share.
- Measured on 2026-10-01 from the process list of a Mac running each of them:
  - OpenAI's ChatGPT app for macOS 26.928.31416, bundle id `com.openai.codex`, runs a codex
    0.159.2 of its own: two processes of
    `ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex`, children
    of the app. That closing its windows leaves it running, and quitting it stops them, is
    read from the app's code and not yet watched.
  - Codex's background app server runs from
    `$CODEX_HOME/packages/app-server-daemon/releases/<version>/bin/codex`, a child of
    launchd, and 0.159.3 has `codex app-server daemon restart`.
  - On macOS, `ps` gives what a process was started as. A `codex` started from a shell by
    its bare name lists as `codex`; one started by its path lists the path. pitboard tells
    the kinds apart by the directories in that path, and a bare name is a `codex` session.
  - No editor with the Codex extension was running there. Its place, a folder named
    `openai.chatgpt-<version>`, is the extension's packaged layout, not measured.
- The ID token names the account: `email`, and under `https://api.openai.com/auth`,
  `chatgpt_account_id` and `chatgpt_user_id`. A Team or Business workspace shares one
  `chatgpt_account_id`, and `chatgpt_user_id` is the person.
- pitboard identifies a Codex account by that pair, with no network call.
- Renewal is `POST https://auth.openai.com/oauth/token` with a JSON body
  `{client_id, grant_type, refresh_token}` and client id `app_EMoamEEZ73f0CkXaXp7hrann`.
  Each token in the answer is written only if present.
- `last_refresh` must be in the login, as an RFC 3339 string, or Codex reads the login as
  having no token data.
- A spent or revoked refresh token answers 400 `invalid_grant`, or one of the older
  `refresh_token_expired`, `refresh_token_reused` and `refresh_token_invalidated`, or 401.
  Any other 400 is not a dead login.
- Usage is `GET https://chatgpt.com/backend-api/wham/usage` with `Authorization: Bearer`
  and `ChatGPT-Account-ID`, and spends no quota.
- The usage answer's shape was read from a live answer. A parser written from the source
  looked for the windows in the wrong place, and returned nothing while the request
  succeeded.
- `CODEX_HOME` moves everything Codex keeps, and an empty one means unset. pitboard's own
  sign-in, the one `enroll --sign-in` runs, sets it to a directory that exists, and runs
  `codex login` from inside it.
- The sign-in starts inside that directory because Codex also reads `.codex/config.toml`
  from a trusted project it starts in. That file could name a keychain store.
- `codex login` revokes the login stored in its home before signing in. It opens the
  browser itself and reads nothing from standard input.

### Claude Desktop

Read from Claude Desktop 2.19675.0 on one Mac on 3 October 2026, read only: the bundle, the
data folder's listing and sizes, the cookie database read without decrypting anything, the
process list, the keychain item's attributes, and the exit code of one read of the key from
a shell with no screen. The register is
`provider/desktop/assumptions.rs`. No literal of the app can be probed for, so no
conformance job reads it.

- The app is `/Applications/Claude.app`, version 2.19675.0 in both `CFBundleShortVersionString`
  and `CFBundleVersion`, bundle id `com.anthropic.claudefordesktop`, signed by team
  `Q6L2SF6YDW`. Its Info.plist carries `ElectronAsarIntegrity`, and its code is in
  `app.asar` and `app.asar.unpacked`.
- The main process is `Contents/MacOS/Claude`. Its helpers run from inside the bundle with
  `--user-data-dir=~/Library/Application Support/Claude`. `Contents/Helpers/chrome-native-host`
  runs apart from the app.
- The data folder, `~/Library/Application Support/Claude`, held 1.3 GB. The account's part
  of it is about 6 MB: `Cookies` (32 KB), `config.json` (10 KB), `Local Storage` (2.3 MB,
  origins `https://claude.ai` and `https://a.claude.ai`), `IndexedDB` (3.2 MB) and
  `Session Storage` (108 KB), with `WebStorage` (44 KB) and `File System` (40 KB), which
  move with it (E2).
- The rest is the machine's: `Cache` (685 MB), `Code Cache` (222 MB), `claude-code/`
  (431 MB), the GPU caches, `Shared Dictionary`, `Partitions`, `Local State` with no
  `os_crypt` key, `Preferences` and `claude_desktop_config.json`.
- Some of the folder is already kept per account by the app itself:
  `claude-code-sessions/<account>/<organisation>`, `local-agent-mode-sessions/`,
  `spaces-present/`, `ant-device-registry.json` and `cowork-enabled-cli-ops.json`. Three
  accounts had signed in on that Mac.
- No `Claude-3p` folder and no saved application state were there. The app's preferences
  plist held only AppKit keys. `Caches`, `HTTPStorages` and `Logs` hold more of the app's
  files outside the data folder.
- `Cookies` is SQLite in `journal_mode=delete`, with `meta.version` 24 and
  `last_compatible` 24. It held 20 cookies, all for claude.ai, each with an empty `value`
  and an `encrypted_value` starting `v10`.
- `sessionKey` and `sessionKeyV3` were 179 bytes each and expired on 30 October 2026, about
  four weeks out. Their ciphertexts had the same SHA-256. `sessionKeyLC`, `sessionKeyV3LC`, `routingHint`, `lastActiveOrg`,
  `__Host-ant_trusted_device` and `anthropic-device-id` were there too.
- While the app ran, `Cookies-journal` was 0 bytes with the same mtime as `Cookies`, and
  there was no `Cookies-wal`.
- `config.json` holds `oauth:tokenCache` and `oauth:tokenCacheV2`, base64 of values
  starting `v10`, and `lastKnownAccountUuid`, a 36-character uuid equal to the
  `ownerAccountId` in `cowork-enabled-cli-ops.json`.
- The keychain item `Claude Safe Storage` is a generic password with account `Claude Key`
  in the login keychain, its `cdat` equal to its `mdat`. Its attributes were read without
  its password.
- `security find-generic-password -w` on that item, run from a background shell of a
  Claude Code session with no screen, exited 36. It was not run over ssh.
- `plan-usage-history.json` is `{version: 2, samples: [{t, org, u: {fh, sd}}]}`. It held 87
  samples for one organisation.
- The home folder and the data folder were on one volume (`stat -f %d`).

Measured on 4 October 2026 on Claude Desktop 2.19675.0, `Cookies` meta version 24, with two
real claude.ai accounts and the data folder backed up first (31,674 entries). Only
fingerprints (12-character SHA-256 prefixes), inodes, exit codes and times were recorded.
Each experiment dates its register entry with the version:

- Log out in the app revokes the session at claude.ai (E1). A byte copy of the listed items
  and config keys, taken before Log out and put back after, was refused: within 5 seconds of
  launch the app deleted `sessionKey`, emptied `oauth:tokenCacheV2` and showed its sign-in
  screen. After Log out `lastKnownAccountUuid` still names the account that left, the
  `sessionKey` row is gone, and `oauth:tokenCacheV2` holds the same 28-character empty value
  as `oauth:tokenCache` (E1b). A switch from that state set 7 leftover items aside and put
  the other account back.
- The item list moves an account whole (E2): after a switch each way, the name,
  conversations, Code tab, Cowork and connectors were the right account's, with nothing of
  the other. One account had no `File System`, so an item on the list may be absent.
- The `sessionKey` ciphertext keeps its fingerprint across quitting and opening the app, for
  both accounts (E3). Only the analytics cookie `_dd_s_v2` changed.
- `lastKnownAccountUuid` is written while the app runs, 35 and 15 seconds after launch on two
  sign-ins, before `sessionKey` reaches the jar at 63 and 31 seconds (E4). The jar is flushed
  about every 30 seconds. Both sign-ins started with no uuid in `config.json`, so whether a
  sign-in replaces the uuid that Log out leaves behind (E1b) is still to be measured (E19).
- A cookie is AES-128-CBC with a key from PBKDF2-SHA1 of the item's password, salt
  `saltysalt`, 1003 rounds, 16 bytes, an IV of sixteen 0x20 bytes, and from meta version 24
  a 32-byte SHA-256 of the host before the value (E5). A `sessionKey` decrypted this way was
  accepted by claude.ai. The item's account is `Claude Key`.
- At quit the app writes `Cookies`, `config.json`, `Local Storage`, `Session Storage`,
  `WebStorage` and `Preferences`, each with the mtime of the second the main process exited
  (E8).
- `oauth:tokenCache` and `oauth:tokenCacheV2` are keyed
  `acct:<lastKnownAccountUuid>|...:<org>:https://api.anthropic.com:<scopes>` and hold
  `token`, `refreshToken`, `expiresAt`, `subscriptionType` and `rateLimitTier` (E9): 1668
  and 1732 characters for one account, 28 and 1500 for the other. These and
  `lastKnownAccountUuid` are the three keys a switch moves, and with the items on the list
  they were enough to move each of two accounts whole (E2). Nobody went through the file's
  other keys one by one to show none of them follows the account.
- `GET claude.ai/api/organizations/<lastActiveOrg>/usage` with the `sessionKey` cookie
  answers 200 with `five_hour` and `seven_day`, each with `utilization` and `resets_at`
  (E10). The history's `fh` and `sd` matched it: 0% and 1% against 0% and 1% for one
  account, 30% and 5% against 30% and 5% for the other, so history readings are verified.
- Nothing ran from the bundle 0, 2, 5 and 30 seconds after a quit but `chrome-native-host`
  (E12), and no crashpad handler or helper was left when the main process exited (E18).
- The app keeps no `SingletonLock`, in its data folder or in `$TMPDIR`, while it runs; its
  main process holds the `LOCK` files of its leveldb stores instead (E14). The lock check
  finds nothing, and the process scan is the only sign the app is open.
- claude.ai answered a request with the user agent `pitboard/<version>` with 200 and no bot
  check (E15). `GET claude.ai/api/account` answers `email_address`, `uuid`, `full_name`,
  `display_name` and `memberships` (E17); pitboard does not ask it yet, so a Claude Desktop
  account shows no email.
- Always Allow left the key's `cdat` and `mdat` at 20260727084307Z (U-K1). Before it, a
  background shell's read exited 36; after it, the same shell read the key without a
  question (U-K1b), so refreshes need nobody at the Mac. Reads over ssh or from a launchd
  job were not tried.
- `security` exits 128 on Deny, and 128 too when a wrong password is typed and Allow chosen;
  51 was never seen (U-K2). Killing it with SIGTERM (exit 143) leaves the question on screen
  (U-K3), so a read that times out can leave a dialog behind. A refresh that timed out does
  not ask again for the same stamp, but that gate is per process: every `enable` asks, and
  two processes refreshing at once can each leave one. Reading the item's attributes without `-w` never prompted, many times
  from a background shell (U-K5).

Not measured yet, and dated `UNVERIFIED` in the register:

- Whether the session's expiry slides forward with use (E11, E13). Both sessions read on 4
  October 2026 expire on 1 November 2026, so the read that settles it comes after that.
- Whether a sign-in replaces a `lastKnownAccountUuid` that Log out left behind (E19). Until
  it is measured, a session pitboard has not seen under a uuid it knows stops with
  `desktop_identity_unconfirmed`, and enrolling that label confirms it.
- Whether reads with Always Allow slow the app down (U-K6; U-K1 read only the stamp), and
  whether an update of the app keeps the item (U-K4).

Not in the register, because pitboard checks it rather than assumes it: whether the data
folder and `~/.pitboard` share a volume on other Macs (E7). Every switch checks it.
