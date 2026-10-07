{ lib, rustPlatform, git, jujutsu, openssh, inetutils, procps, stdenv, makeWrapper, python3 }:
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
  nativeBuildInputs = [ makeWrapper ] ++ lib.optionals stdenv.hostPlatform.isDarwin [ rustPlatform.bindgenHook ];
  nativeCheckInputs = [ git jujutsu python3 inetutils ] ++ lib.optionals stdenv.hostPlatform.isLinux [ procps ];
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
    runHook postInstallCheck
  '';
  meta = {
    description = "Run committed repository recipes locally or over SSH with durable receipts";
    mainProgram = "ferry";
    platforms = lib.platforms.unix;
  };
}
