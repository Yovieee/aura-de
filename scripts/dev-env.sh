#!/usr/bin/env bash
# Source this file when using a user-local native dependency sysroot.
# Normal system package installations do not need it.
aura_cloud_setup="${AURA_CLOUD_SETUP_ROOT:-/workspace/.cloud-setup}"
if [[ -x "$aura_cloud_setup/cargo/bin/cargo" && -d "$aura_cloud_setup/rustup" ]]; then
    export CARGO_HOME="${CARGO_HOME:-$aura_cloud_setup/cargo}"
    export RUSTUP_HOME="${RUSTUP_HOME:-$aura_cloud_setup/rustup}"
    export PATH="$CARGO_HOME/bin:$PATH"
fi
unset aura_cloud_setup
if [[ -n "${AURA_SYSROOT:-}" ]]; then
    aura_sysroot_prefix="${AURA_SYSROOT%/}/usr"
    aura_triplet="$(cc -dumpmachine)"
    export PKG_CONFIG_SYSROOT_DIR="${PKG_CONFIG_SYSROOT_DIR:-$AURA_SYSROOT}"
    export PKG_CONFIG_PATH="$aura_sysroot_prefix/lib/$aura_triplet/pkgconfig:$aura_sysroot_prefix/lib/pkgconfig:$aura_sysroot_prefix/share/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
    export LIBRARY_PATH="$aura_sysroot_prefix/lib/$aura_triplet:$aura_sysroot_prefix/lib${LIBRARY_PATH:+:$LIBRARY_PATH}"
    export LD_LIBRARY_PATH="$aura_sysroot_prefix/lib/$aura_triplet:$aura_sysroot_prefix/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
    export PATH="$aura_sysroot_prefix/bin:$PATH"
    if [[ -f "$aura_sysroot_prefix/share/vulkan/icd.d/lvp_icd.$(uname -m).json" && -z "${VK_DRIVER_FILES:-}" ]]; then
        export VK_DRIVER_FILES="$aura_sysroot_prefix/share/vulkan/icd.d/lvp_icd.$(uname -m).json"
    fi
    if [[ -d "$aura_sysroot_prefix/lib/$aura_triplet/dri" && -z "${LIBGL_DRIVERS_PATH:-}" ]]; then
        export LIBGL_DRIVERS_PATH="$aura_sysroot_prefix/lib/$aura_triplet/dri"
    fi
    if [[ -f "$aura_sysroot_prefix/share/glvnd/egl_vendor.d/50_mesa.json" && -z "${__EGL_VENDOR_LIBRARY_FILENAMES:-}" ]]; then
        export __EGL_VENDOR_LIBRARY_FILENAMES="$aura_sysroot_prefix/share/glvnd/egl_vendor.d/50_mesa.json"
    fi
    if [[ -z "${LIBCLANG_PATH:-}" ]]; then
        for aura_clang_dir in "$aura_sysroot_prefix"/lib/llvm-*/lib; do
            if [[ -f "$aura_clang_dir/libclang.so" ]]; then export LIBCLANG_PATH="$aura_clang_dir"; fi
        done
    fi
    if [[ -n "${LIBCLANG_PATH:-}" && -z "${BINDGEN_EXTRA_CLANG_ARGS:-}" ]]; then
        for aura_clang_resource in "$LIBCLANG_PATH"/clang/*; do
            if [[ -d "$aura_clang_resource/include" ]]; then
                export BINDGEN_EXTRA_CLANG_ARGS="-resource-dir=$aura_clang_resource"
            fi
        done
    fi
    if [[ -d "$aura_sysroot_prefix/share/pipewire" && -z "${PIPEWIRE_CONFIG_DIR:-}" ]]; then
        export PIPEWIRE_CONFIG_DIR="$aura_sysroot_prefix/share/pipewire"
    fi
    if [[ -d "$aura_sysroot_prefix/lib/$aura_triplet/pipewire-0.3" && -z "${PIPEWIRE_MODULE_DIR:-}" ]]; then
        export PIPEWIRE_MODULE_DIR="$aura_sysroot_prefix/lib/$aura_triplet/pipewire-0.3"
    fi
    if [[ -z "${SPA_PLUGIN_DIR:-}" ]]; then
        if [[ -d "$aura_sysroot_prefix/lib/$aura_triplet/spa-0.2" ]]; then
            export SPA_PLUGIN_DIR="$aura_sysroot_prefix/lib/$aura_triplet/spa-0.2"
        elif [[ -d "/usr/lib/$aura_triplet/spa-0.2" ]]; then
            export SPA_PLUGIN_DIR="/usr/lib/$aura_triplet/spa-0.2"
        fi
    fi
    unset aura_sysroot_prefix aura_triplet aura_clang_dir aura_clang_resource
fi
