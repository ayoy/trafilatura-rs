#!/usr/bin/env bash
# Wraps the per-target trafilatura_uniffi dylibs into TrafilaturaFFI.framework bundles and an XCFramework.
#
# A dynamic framework keeps the Rust standard library private to it. A static library would clash with any
# other Rust static library linked into the same app (duplicate `rust_eh_personality`), and its root-level
# `module.modulemap` would clash with other binary targets that ship one.
#
# usage: build-xcframework.sh --version X.Y.Z --bindings DIR --output DIR \
#          --macos DYLIB... --ios DYLIB... --ios-sim DYLIB...
#   --bindings  directory with the uniffi-generated TrafilaturaFFI.h
#   --output    directory that receives TrafilaturaFFI.xcframework
set -euo pipefail

NAME=TrafilaturaFFI
MACOS_MIN=12.0
IOS_MIN=15.0

version=""
bindings=""
output=""
macos=()
ios=()
ios_sim=()
slice=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --version) version="$2"; shift 2 ;;
        --bindings) bindings="$2"; shift 2 ;;
        --output) output="$2"; shift 2 ;;
        --macos|--ios|--ios-sim) slice="$1"; shift ;;
        *)
            case "$slice" in
                --macos) macos+=("$1") ;;
                --ios) ios+=("$1") ;;
                --ios-sim) ios_sim+=("$1") ;;
                *) echo "unexpected argument: $1" >&2; exit 1 ;;
            esac
            shift ;;
    esac
done
[[ -n "$version" && -n "$bindings" && -n "$output" ]] || { echo "--version, --bindings and --output are required" >&2; exit 1; }
[[ ${#macos[@]} -gt 0 && ${#ios[@]} -gt 0 && ${#ios_sim[@]} -gt 0 ]] || { echo "dylibs for all three slices are required" >&2; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

write_info_plist() {
    local path="$1" platform="$2" min_key="$3" min_version="$4"
    cat > "$path" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key><string>en</string>
    <key>CFBundleExecutable</key><string>$NAME</string>
    <key>CFBundleIdentifier</key><string>rs.trafilatura.$NAME</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleName</key><string>$NAME</string>
    <key>CFBundlePackageType</key><string>FMWK</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>CFBundleSupportedPlatforms</key><array><string>$platform</string></array>
    <key>$min_key</key><string>$min_version</string>
</dict>
</plist>
EOF
}

write_module() {
    local dir="$1"
    mkdir -p "$dir/Headers" "$dir/Modules"
    cp "$bindings/$NAME.h" "$dir/Headers/"
    cat > "$dir/Modules/module.modulemap" <<EOF
framework module $NAME {
    umbrella header "$NAME.h"
    export *
}
EOF
}

# install_name_tool fails when the new id is longer than the header padding allows,
# so the id is shortened before lipo while each dylib is still single-arch.
make_binary() {
    local out="$1" install_name="$2"; shift 2
    local thin=()
    for dylib in "$@"; do
        local copy="$work/$(uuidgen).dylib"
        cp "$dylib" "$copy"
        install_name_tool -id "$install_name" "$copy"
        thin+=("$copy")
    done
    lipo -create "${thin[@]}" -output "$out"
    # CI artifacts don't keep file modes, so the dylibs may arrive without the executable bit.
    chmod 755 "$out"
    check_string_pool_alignment "$out"
}

# `strip` (run by cargo's default `strip = "debuginfo"`, or by hand) leaves the LINKEDIT string pool misaligned,
# and the Xcode linker then refuses to link against the dylib. Build with `CARGO_PROFILE_RELEASE_STRIP=false`
# and let the linker drop the symbols instead (`-C link-arg=-Wl,-x -C link-arg=-Wl,-S`).
check_string_pool_alignment() {
    local binary="$1"
    for arch in $(lipo -archs "$binary"); do
        local stroff
        stroff="$(otool -arch "$arch" -l "$binary" | awk '$1 == "stroff" { print $2; exit }')"
        if (( stroff % 8 != 0 )); then
            echo "$binary ($arch): misaligned LINKEDIT string pool at $stroff; was the dylib stripped?" >&2
            exit 1
        fi
    done
}

# Editing the binary invalidates the linker's ad-hoc signature, and arm64 refuses to load unsigned code.
# Apps re-sign embedded frameworks with their own identity.
sign() {
    codesign --force --sign - "$1" >&2
}

# macOS frameworks use the versioned bundle layout; iOS frameworks are flat.
make_macos_framework() {
    local fw="$work/macos/$NAME.framework"
    local v="$fw/Versions/A"
    mkdir -p "$v/Resources"
    make_binary "$v/$NAME" "@rpath/$NAME.framework/Versions/A/$NAME" "$@"
    write_module "$v"
    write_info_plist "$v/Resources/Info.plist" MacOSX LSMinimumSystemVersion "$MACOS_MIN"
    ln -s A "$fw/Versions/Current"
    for item in "$NAME" Headers Modules Resources; do
        ln -s "Versions/Current/$item" "$fw/$item"
    done
    sign "$v"
    echo "$fw"
}

make_ios_framework() {
    local label="$1" platform="$2"; shift 2
    local fw="$work/$label/$NAME.framework"
    mkdir -p "$fw"
    make_binary "$fw/$NAME" "@rpath/$NAME.framework/$NAME" "$@"
    write_module "$fw"
    write_info_plist "$fw/Info.plist" "$platform" MinimumOSVersion "$IOS_MIN"
    sign "$fw"
    echo "$fw"
}

macos_fw="$(make_macos_framework "${macos[@]}")"
ios_fw="$(make_ios_framework ios iPhoneOS "${ios[@]}")"
ios_sim_fw="$(make_ios_framework ios-sim iPhoneSimulator "${ios_sim[@]}")"

mkdir -p "$output"
rm -rf "$output/$NAME.xcframework"
xcodebuild -create-xcframework \
    -framework "$macos_fw" \
    -framework "$ios_fw" \
    -framework "$ios_sim_fw" \
    -output "$output/$NAME.xcframework"
