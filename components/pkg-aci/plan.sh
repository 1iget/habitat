# shellcheck disable=2154
pkg_origin=core
pkg_name=hab-pkg-aci
pkg_version=$(cat "$PLAN_CONTEXT/../../VERSION")
pkg_maintainer="The Habitat Maintainers <humans@habitat.sh>"
pkg_license=("Apache-2.0")
pkg_source=nosuchfile.tar.gz
pkg_deps=(core/coreutils/8.25/20170513213226
          core/findutils/4.4.2/20170513214305
          core/gawk/4.1.3/20170513213646
          core/grep/2.22/20170513213444
          core/bash/4.3.42/20170513213519
          core/acbuild/0.4.0/20171109161533
          core/hab-studio)
pkg_build_deps=()
pkg_bin_dirs=(bin)

program=$pkg_name

do_build() {
  cp -v "${PLAN_CONTEXT}/bin/${program}.sh" "${program}"


  # Use the bash from our dependency list as the shebang. Also, embed the
  # release version of the program.
  sed \
    -e "s,#!/bin/bash$,#!$(pkg_path_for bash)/bin/bash," \
    -e "s,@author@,$pkg_maintainer,g" \
    -e "s,@version@,$pkg_version/$pkg_release,g" \
    -i $program
}

do_install() {
  install -v -D "$program" "$pkg_prefix/bin/$program"
}

# Turn the remaining default phases into no-ops

do_download() {
  return 0
}

do_verify() {
  return 0
}

do_unpack() {
  return 0
}

do_prepare() {
  return 0
}
