{ lib, rustPlatform, git, jujutsu, openssh, inetutils, procps, util-linux, stdenv, makeWrapper, python3, sourceRev ? null, checkType ? "release" }:
let
  version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
in rustPlatform.buildRustPackage {
  pname = "ferry";
  inherit version;
  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./rust-toolchain.toml ./src ./tests ];
  };
  cargoLock.lockFile = ./Cargo.lock;
  # The shipped package's tests run in release, where the controls that
  # need fault points skip. `checkType = "debug"` (the flake's `debug`
  # check) runs them all, and claims them: a skip there fails.
  inherit checkType;
  env = lib.optionalAttrs (sourceRev != null) { FERRY_SOURCE_REV = sourceRev; }
    // lib.optionalAttrs (checkType == "debug") { FERRY_TEST_STRICT = "1"; };
  nativeBuildInputs = [ makeWrapper ] ++ lib.optionals stdenv.hostPlatform.isDarwin [ rustPlatform.bindgenHook ];
  nativeCheckInputs = [ git jujutsu python3 inetutils ] ++ lib.optionals stdenv.hostPlatform.isLinux [ procps util-linux ];
  doCheck = true;
  # The real session tests inspect and signal only processes they start.
  # This is the same Darwin sandbox allowance used by nixpkgs' fish tests.
  sandboxProfile = lib.optionalString stdenv.hostPlatform.isDarwin ''
    (allow mach-lookup mach-task-name)
    (allow signal (target children))
  '';
  postInstall = ''
    wrapProgram $out/bin/ferry --prefix PATH : ${lib.makeBinPath ([ git openssh inetutils ] ++ lib.optionals stdenv.hostPlatform.isLinux [ procps ])}
  '';
  doInstallCheck = true;
  installCheckPhase = ''
    runHook preInstallCheck
    test "$($out/bin/ferry --version)" = "ferry ${version}"
    $out/bin/ferry --help >/dev/null
    $out/bin/ferry host-info | grep -F '"source_rev":"${if sourceRev == null then "unpinned" else sourceRev}"'
    runHook postInstallCheck
  '';
  meta = {
    description = "Run committed repository recipes locally or over SSH with durable receipts";
    homepage = "https://github.com/flowerornament/ferry";
    license = with lib.licenses; [ mit asl20 ];
    mainProgram = "ferry";
    platforms = lib.platforms.unix;
  };
}
