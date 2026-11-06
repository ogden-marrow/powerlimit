cmake_minimum_required(VERSION 3.16)
project(powerlimit VERSION 0.1.0 LANGUAGES NONE)

include(GNUInstallDirs)
find_program(CARGO cargo REQUIRED)

set(POLKIT_ACTIONS_DIR "/usr/share/polkit-1/actions" CACHE PATH "polkit actions directory")
set(BIN ${CMAKE_CURRENT_BINARY_DIR}/cargo/release/powerlimit)

add_custom_command(
  OUTPUT ${BIN}
  COMMAND ${CMAKE_COMMAND} -E env CARGO_TARGET_DIR=${CMAKE_CURRENT_BINARY_DIR}/cargo
          ${CARGO} build --release --manifest-path ${CMAKE_CURRENT_SOURCE_DIR}/Cargo.toml
  DEPENDS src/main.rs src/gui.rs src/style.css src/style-dark.css Cargo.toml
  COMMENT "cargo build --release")
add_custom_target(powerlimit ALL DEPENDS ${BIN})

configure_file(data/org.powerlimit.policy.in org.powerlimit.policy @ONLY)

install(PROGRAMS ${BIN} DESTINATION ${CMAKE_INSTALL_BINDIR})
install(FILES data/io.github.powerlimit.desktop DESTINATION ${CMAKE_INSTALL_DATADIR}/applications)
foreach(n 16 32 48 64 128 256 512)
  install(FILES data/icons/hicolor/${n}x${n}/apps/powerlimit.png DESTINATION ${CMAKE_INSTALL_DATADIR}/icons/hicolor/${n}x${n}/apps)
endforeach()
install(FILES data/powerlimit.svg DESTINATION ${CMAKE_INSTALL_DATADIR}/icons/hicolor/scalable/apps)
install(FILES ${CMAKE_CURRENT_BINARY_DIR}/org.powerlimit.policy DESTINATION ${POLKIT_ACTIONS_DIR})

add_custom_target(uninstall
  COMMAND xargs rm -fv < ${CMAKE_CURRENT_BINARY_DIR}/install_manifest.txt
  COMMENT "Removing installed files")
