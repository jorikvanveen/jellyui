{
  description = "Jellyui audio experiments";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forEachSystem = nixpkgs.lib.genAttrs systems;
    in
    {
      devShells = forEachSystem (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.mkShell {
            packages = with pkgs; [ cargo rustc rustfmt clippy pipewire wireplumber ];
            nativeBuildInputs = [ pkgs.pkg-config pkgs.rustPlatform.bindgenHook ];
            buildInputs = [ pkgs.pipewire ];
          };
        });

      packages = forEachSystem (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "jellyui-rate-switch";
            version = "0.1.0";
            src = pkgs.lib.cleanSource self;
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = [ pkgs.pkg-config pkgs.rustPlatform.bindgenHook ];
            buildInputs = [ pkgs.pipewire ];
            meta.mainProgram = "rate-switch";
          };
        });
    };
}
