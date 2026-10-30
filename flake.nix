{
  description = "Jellyui Linux music player for Jellyfin";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.crane.url = "github:ipetkov/crane";

  outputs = { self, nixpkgs, crane }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forEachSystem = nixpkgs.lib.genAttrs systems;
      libraries = pkgs: with pkgs; [
        fontconfig freetype libxkbcommon libxcb wayland
        libGL vulkan-loader openssl zlib pipewire
      ];
    in
    {
      devShells = forEachSystem (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.mkShell {
            packages = with pkgs; [ cargo rustc rustfmt clippy dbus ];
            nativeBuildInputs = [ pkgs.pkg-config pkgs.rustPlatform.bindgenHook ];
            buildInputs = libraries pkgs;
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (libraries pkgs);
          };
        });

      packages = forEachSystem (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          craneLib = crane.mkLib pkgs;
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter = path: type:
              craneLib.filterCargoSources path type
              || (pkgs.lib.hasInfix "/assets/icons/" path && pkgs.lib.hasSuffix ".svg" path)
              || (pkgs.lib.hasInfix "/tests/fixtures/" path
                && (pkgs.lib.hasSuffix ".flac" path || pkgs.lib.hasSuffix ".mp3" path));
          };
          commonArgs = {
            inherit src;
            pname = "jellyui";
            version = "0.1.0";
            strictDeps = true;
            nativeBuildInputs = [ pkgs.pkg-config pkgs.rustPlatform.bindgenHook ];
            buildInputs = libraries pkgs;
          };
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
        in {
          # Cache compiled dependencies independently of application source changes.
          inherit cargoArtifacts;
          default = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            nativeCheckInputs = [ pkgs.dbus ];
            nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ pkgs.makeWrapper ];
            postInstall = ''
              install -Dm644 ${./licenses/FILLED-LUCIDE.txt} $out/share/licenses/jellyui/FILLED-LUCIDE.txt
              install -Dm644 ${./assets/jellyui.desktop} $out/share/applications/jellyui.desktop
              substituteInPlace $out/share/applications/jellyui.desktop \
                --replace-fail 'Exec=jellyui' "Exec=$out/bin/jellyui"
              wrapProgram $out/bin/jellyui \
                --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath (libraries pkgs)}
            '';
            meta.mainProgram = "jellyui";
          });
        });
    };
}
