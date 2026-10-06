#!/bin/sh
# Builds Pitboard.app, universal, from the Xcode project XcodeGen generates out of
# project.yml and the core's XCFramework, with the command line inside it. This is the one way the app is built: here, in CI and in a
# release.
#
# Signing: set SIGN_IDENTITY to a Developer ID Application identity to make a build others
# can run; without it the app is signed ad-hoc, which is enough on the machine that built it.
#
# Updates: set SPARKLE_PUBLIC_KEY to the EdDSA public key that signs the appcast, and
# SPARKLE_FEED_URL to override where it is read from. Without the key the app ships with no
# updater, which is what a build from a clone wants.
set -eu

cd "$(dirname "$0")/../../.."
export MACOSX_DEPLOYMENT_TARGET=14.0
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
[ -n "$version" ] || { echo "no version in Cargo.toml" >&2; exit 1; }
identity=${SIGN_IDENTITY:--}
# GitHub serves the newest release's assets at a fixed address, so the feed has one even
# before the site does.
FEED=https://github.com/datlechin/pitboard/releases/latest/download/appcast.xml
app=apps/macos/build/Pitboard.app
# Sparkle decides what is newer by CFBundleVersion, so it counts up with the version
# rather than staying at whatever the project says.
rest=${version#*.}
build=$((${version%%.*} * 10000 + ${rest%%.*} * 100 + ${rest#*.}))

# This clears apps/macos/build and makes it again, so nothing from a previous build survives.
./apps/macos/scripts/build-xcframework.sh

# The project is generated, never edited, so the one built is the one project.yml says.
command -v xcodegen >/dev/null || {
    echo "XcodeGen generates the Xcode project: install it with \`brew install xcodegen\`" >&2
    exit 1
}
xcodegen generate --spec apps/macos/project.yml --quiet

# Unsigned, because the bundle is not finished: the command line, the icon and the man
# pages go in after this, and a signature covers what is inside it. Sparkle is the version
# and revision the committed Package.resolved pins or the build stops, since the app's bill
# of materials reads it from there. Packages are cloned under apps/macos/build, where the release finds Sparkle's tools.
xcodebuild -project apps/macos/Pitboard.xcodeproj -scheme Pitboard -configuration Release \
    -destination 'generic/platform=macOS' \
    -derivedDataPath apps/macos/build/DerivedData \
    -clonedSourcePackagesDirPath apps/macos/build/SourcePackages \
    -onlyUsePackageVersionsFromResolvedFile -quiet \
    ARCHS="arm64 x86_64" ONLY_ACTIVE_ARCH=NO \
    MARKETING_VERSION="$version" CURRENT_PROJECT_VERSION="$build" \
    CODE_SIGNING_ALLOWED=NO build
ditto apps/macos/build/DerivedData/Build/Products/Release/Pitboard.app "$app"
[ -d "$app/Contents/Frameworks/Sparkle.framework" ] || {
    echo "Xcode did not embed Sparkle.framework" >&2
    exit 1
}
# The Share extension, which hands a page's claude.ai or chatgpt.com link to the app, built
# for both kinds of Mac like the app. The release claims pitboard://, which the extension
# opens, and both read it from PitboardURLScheme; a debug build claims pitboard-debug://.
appex=$app/Contents/PlugIns/PitboardShare.appex
[ -d "$appex" ] || {
    echo "Xcode did not embed PitboardShare.appex" >&2
    exit 1
}
for arch in arm64 x86_64; do
    lipo "$appex/Contents/MacOS/PitboardShare" -verify_arch "$arch"
done
for scheme in \
    "$(plutil -extract CFBundleURLTypes.0.CFBundleURLSchemes.0 raw "$app/Contents/Info.plist")" \
    "$(plutil -extract PitboardURLScheme raw "$app/Contents/Info.plist")" \
    "$(plutil -extract PitboardURLScheme raw "$appex/Contents/Info.plist")"; do
    [ "$scheme" = pitboard ] || {
        echo "the app or its extension names $scheme://, not pitboard://" >&2
        exit 1
    }
done

# The command line comes inside the app, so one update moves both, and the renewal
# schedule has a Pitboard to run: the app has no renewal of its own.
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --locked --release -p pitboard --target "$target"
done
cli=apps/macos/build/cli-universal
lipo -create target/aarch64-apple-darwin/release/pitboard \
    target/x86_64-apple-darwin/release/pitboard -output "$cli"
# Helpers, which is where a bundle keeps a tool that is not its main program. In MacOS it
# would be the same file as Pitboard on a case-insensitive volume, and overwrite it.
mkdir -p "$app/Contents/Helpers"
cp "$cli" "$app/Contents/Helpers/pitboard"

mkdir -p "$app/Contents/Resources"
swift apps/macos/scripts/make-icon.swift apps/macos/build
iconutil --convert icns --output "$app/Contents/Resources/AppIcon.icns" \
    apps/macos/build/AppIcon.iconset
# The Share menu shows the extension's own icon, which is the app's.
mkdir -p "$appex/Contents/Resources"
cp "$app/Contents/Resources/AppIcon.icns" "$appex/Contents/Resources/AppIcon.icns"

# The man page and completions for whatever links the command line onto PATH, written by
# the build that ships so they describe it, and before signing, since the app's signature
# covers its Resources. It runs from where lipo left it: macOS scans a bundle the first
# time a program inside it runs, and a write into the bundle during the scan fails.
mkdir -p "$app/Contents/Resources/man" "$app/Contents/Resources/completions"
"$cli" manpage > "$app/Contents/Resources/man/pitboard.1"
for shell in bash zsh fish; do
    "$cli" completions "$shell" > "$app/Contents/Resources/completions/pitboard.$shell"
done

if [ -n "${SPARKLE_PUBLIC_KEY:-}" ]; then
    plist=$app/Contents/Info.plist
    /usr/libexec/PlistBuddy -c "Add :SUPublicEDKey string $SPARKLE_PUBLIC_KEY" "$plist"
    /usr/libexec/PlistBuddy -c \
        "Add :SUFeedURL string ${SPARKLE_FEED_URL:-$FEED}" "$plist"
    /usr/libexec/PlistBuddy -c "Add :SUEnableAutomaticChecks bool true" "$plist"
fi

# Notarization wants the hardened runtime and a secure timestamp; an ad-hoc signature can
# have neither. Under the hardened runtime it would refuse to load its own framework,
# since ad-hoc signatures share no team.
if [ "$identity" = "-" ]; then
    options="--timestamp=none"
else
    options="--timestamp --options runtime"
fi
# Sparkle's helpers are signed before the framework, and the framework and the command
# line before the app: a signature covers what is inside it, so the inside has to be
# settled first. A bare executable has no Info.plist to take an identifier from, so the
# command line is given one under the app's.
# shellcheck disable=SC2086 # $options is a list of flags.
codesign --force $options --sign "$identity" -i com.usepitboard.Pitboard.cli \
    "$app/Contents/Helpers/pitboard"
for helper in "$app/Contents/Frameworks/Sparkle.framework/Versions/"*/XPCServices/*.xpc \
    "$app/Contents/Frameworks/Sparkle.framework/Versions/"*/Updater.app \
    "$app/Contents/Frameworks/Sparkle.framework/Versions/"*/Autoupdate; do
    # shellcheck disable=SC2086 # $options is a list of flags.
    [ -e "$helper" ] && codesign --force $options --sign "$identity" "$helper"
done
# shellcheck disable=SC2086 # $options is a list of flags.
codesign --force $options --sign "$identity" "$app/Contents/Frameworks/Sparkle.framework"
# An app extension must be sandboxed, or macOS refuses to run it, and a signature made
# without --entitlements carries none. It is signed before the app, and the app's own
# signature is made without --deep, so it keeps this one. The key is written with its dots
# escaped, which plutil otherwise reads as a path of four keys.
# shellcheck disable=SC2086 # $options is a list of flags.
codesign --force $options --sign "$identity" \
    --entitlements apps/macos/ShareExtension/PitboardShare.entitlements "$appex"
sandboxed=$(codesign -d --entitlements - --xml "$appex" 2>/dev/null |
    plutil -extract 'com\.apple\.security\.app-sandbox' raw - 2>/dev/null || true)
[ "$sandboxed" = true ] || {
    echo "the share extension is not sandboxed" >&2
    exit 1
}
# The extension checks a link with pitboard-share-ffi and links nothing of the core, whose
# bindings would bring every export in. The app links the core and not the extension's
# library: each Rust static library carries its own copy of Rust's standard library, and two
# must never meet in one binary. Each binary must also hold the library it links, so one
# with no symbols to read fails here rather than passing.
links() {
    nm -a "$1" | grep -q "$2"
}
extension_binary=$appex/Contents/MacOS/PitboardShare
app_binary=$app/Contents/MacOS/Pitboard
links "$extension_binary" uniffi_pitboard_share_ffi_ || {
    echo "the share extension does not link pitboard-share-ffi" >&2
    exit 1
}
if links "$extension_binary" uniffi_pitboard_ffi_; then
    echo "the share extension links the core" >&2
    exit 1
fi
links "$app_binary" uniffi_pitboard_ffi_ || {
    echo "the app does not link the core" >&2
    exit 1
}
if links "$app_binary" uniffi_pitboard_share_ffi_; then
    echo "the app links the share extension's library" >&2
    exit 1
fi
# Re-signing must retain the permission to ask for Terminal automation.
# shellcheck disable=SC2086
codesign --force $options --sign "$identity" \
    --entitlements apps/macos/App/Pitboard.entitlements "$app"
automation=$(codesign -d --entitlements - --xml "$app" 2>/dev/null |
    plutil -extract 'com\.apple\.security\.automation\.apple-events' raw - 2>/dev/null || true)
[ "$automation" = true ] || {
    echo "the app cannot request Terminal automation" >&2
    exit 1
}
codesign --verify --strict --deep "$app"
echo "built $app ($version, $(lipo -archs "$app/Contents/MacOS/Pitboard"))"
