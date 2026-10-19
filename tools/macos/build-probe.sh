#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Builds tools/macos/vulkan_submit_probe against the Vulkan loader (Homebrew's vulkan-loader,
# vulkan-headers and molten-vk, or the LunarG SDK through VULKAN_SDK).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
prefix=${VULKAN_SDK:-$(brew --prefix 2>/dev/null || echo /usr/local)}
out=${1:-$here/../../target/vulkan_submit_probe}
clang++ -std=c++17 -O2 -Wall -Wno-missing-field-initializers \
	-I"$prefix/include" "$here/vulkan_submit_probe.cpp" \
	-L"$prefix/lib" -Wl,-rpath,"$prefix/lib" -lvulkan -o "$out"
echo "built $out"
