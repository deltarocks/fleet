{
  settings.global.excludes = [
    "*.adoc"
    "*.png"
    "crates/nixlike/fuzz/.gitignore"
  ];

  programs.nixfmt.enable = true;
  programs.shfmt.enable = true;
  programs.rustfmt.enable = true;
  programs.taplo.enable = true;
  programs.clang-format.enable = true;
}
