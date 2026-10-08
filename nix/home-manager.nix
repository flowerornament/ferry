{ self, src }:
{ config, lib, pkgs, ... }:
let
  defaultPackage = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
  isRelativeSkillTarget = target: target != "" && builtins.substring 0 1 target != "/";
in
{
  options.programs.ferry = {
    enable = lib.mkEnableOption "ferry, exact-commit recipe runs with receipts";

    package = lib.mkOption {
      type = lib.types.package;
      default = defaultPackage;
      defaultText = lib.literalExpression "ferry package from this flake";
      description = "The ferry package to install.";
    };

    skill = {
      enable = lib.mkEnableOption "ferry skill symlink management";
      targets = lib.mkOption {
        type = lib.types.listOf (lib.types.addCheck lib.types.str isRelativeSkillTarget);
        default = [ ".agents/skills/ferry" ];
        example = lib.literalExpression ''[ ".agents/skills/ferry" ".claude/skills/ferry" ]'';
        description = "Home-relative paths where Home Manager should symlink ferry's `skills/ferry` directory.";
      };
    };
  };

  config =
    let
      cfg = config.programs.ferry;
      hasUniqueSkillTargets = builtins.length cfg.skill.targets == builtins.length (lib.unique cfg.skill.targets);
      skillFiles = lib.genAttrs cfg.skill.targets (_: { source = "${src}/skills/ferry"; });
    in
    lib.mkMerge [
      {
        assertions = [
          { assertion = !cfg.skill.enable || cfg.enable;
            message = "programs.ferry.skill.enable requires programs.ferry.enable = true"; }
          { assertion = !cfg.skill.enable || hasUniqueSkillTargets;
            message = "programs.ferry.skill.targets must not contain duplicate paths"; }
        ];
      }
      (lib.mkIf cfg.enable { home.packages = [ cfg.package ]; })
      (lib.mkIf (cfg.enable && cfg.skill.enable) { home.file = skillFiles; })
    ];
}
