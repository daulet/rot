#!/usr/bin/env bash
set -euo pipefail

usage() {
	echo "usage: package-deb.sh --binary PATH --version X.Y.Z --arch amd64|arm64 --out-dir DIR" >&2
	exit 2
}

binary=
version=
architecture=
out_dir=

while (($#)); do
	case "$1" in
	--binary)
		(($# >= 2)) || usage
		binary=$2
		shift 2
		;;
	--version)
		(($# >= 2)) || usage
		version=$2
		shift 2
		;;
	--arch)
		(($# >= 2)) || usage
		architecture=$2
		shift 2
		;;
	--out-dir)
		(($# >= 2)) || usage
		out_dir=$2
		shift 2
		;;
	*) usage ;;
	esac
done

[[ -n "$binary" && -n "$version" && -n "$architecture" && -n "$out_dir" ]] || usage
[[ -f "$binary" && -x "$binary" ]] || {
	echo "binary is not an executable file: $binary" >&2
	exit 2
}
[[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || {
	echo "invalid version: $version" >&2
	exit 2
}
[[ "$architecture" == amd64 || "$architecture" == arm64 ]] || {
	echo "unsupported Debian architecture: $architecture" >&2
	exit 2
}
command -v dpkg-deb >/dev/null || {
	echo "dpkg-deb is required" >&2
	exit 2
}

mkdir -p "$out_dir"
package_root=$(mktemp -d)
trap 'rm -rf "$package_root"' EXIT

mkdir -p "$package_root/usr/bin" "$package_root/usr/share/doc/rot"
install -m 755 "$binary" "$package_root/usr/bin/rot"
install -m 644 LICENSE-MIT "$package_root/usr/share/doc/rot/LICENSE-MIT"
install -m 644 LICENSE-APACHE "$package_root/usr/share/doc/rot/LICENSE-APACHE"
copyright="$package_root/usr/share/doc/rot/copyright"
{
	printf '%s\n\n' \
		'Copyright (c) 2026 Rot contributors' \
		'Source: https://github.com/daulet/rot' \
		'Rot is distributed under the MIT or Apache License 2.0, at your option.' \
		'The complete MIT terms follow:'
	cat LICENSE-MIT
	printf '\n%s\n' 'The complete Apache License 2.0 terms follow:'
	cat LICENSE-APACHE
} >"$copyright"
chmod 0644 "$copyright"
installed_size=$(du -sk "$package_root/usr" | awk '{print $1}')

mkdir -p "$package_root/DEBIAN"
control="$package_root/DEBIAN/control"
printf '%s\n' \
	'Package: rot' \
	"Version: $version" \
	"Architecture: $architecture" \
	'Maintainer: Rot contributors <daulet@users.noreply.github.com>' \
	"Installed-Size: $installed_size" \
	'Section: utils' \
	'Priority: optional' \
	'Homepage: https://github.com/daulet/rot' \
	'Description: Fast, configuration-aware Rust source metrics' \
	' Rot separates production and test code and reports authored complexity.' \
	>"$control"
chmod 0644 "$control"

if [[ -n "${SOURCE_DATE_EPOCH:-}" ]]; then
	[[ "$SOURCE_DATE_EPOCH" =~ ^[0-9]+$ ]] || {
		echo "SOURCE_DATE_EPOCH must be an integer" >&2
		exit 2
	}
	find "$package_root" -exec touch -d "@$SOURCE_DATE_EPOCH" {} +
fi

output="$out_dir/rot_${version}_${architecture}.deb"
dpkg-deb --root-owner-group --build "$package_root" "$output"
printf '%s\n' "$output"
