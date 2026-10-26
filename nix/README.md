# Nix Flake Usage

## Run

```bash
nix run github:re2zero/hopdeck

# With params
nix run github:re2zero/hopdeck -- --help

```

## Home-manager module

Add input:

```nix
inputs = {
    hopdeck.url = "github:re2zero/hopdeck";
}
```

Optional: once a [cachix cache](https://app.cachix.org/cache/hopdeck) is set up
for this fork, add it as a binary cache for faster package installs.

```nix
nixConfig = {
    extra-substituters = [
        "https://hopdeck.cachix.org/"
    ];
    extra-trusted-public-keys = [
        # TODO: add the public key for the `hopdeck` cachix cache once it
        # exists, e.g. "hopdeck.cachix.org-1:<public-key>"
    ];
};
```

Enable HopDeck:

``` nix
{
  inputs,
  ...
}: {
  # Add the Home Manager module
  imports = [inputs.hopdeck.homeManagerModules.default];

  programs.hopdeck = {
    enable = true;
    # systemd = false;
    # package = inputs.hopdeck.packages.${pkgs.stdenv.hostPlatform.system}.default
    # Optional configuration in nix syntax, see config.toml for available options
    # settings = { };
    };
  };
}

```
