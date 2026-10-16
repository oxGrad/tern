# Scratch dir for testenv-* recipes: isolated XDG dirs so testing never
# touches the real ~/.config/tern or ~/.local/share/tern.
testenv_dir := justfile_directory() / ".testenv"
testenv_env := "XDG_CONFIG_HOME=" + testenv_dir / "config" + " XDG_DATA_HOME=" + testenv_dir / "data"

testenv-init:
  {{ testenv_env }} cargo run -- init

testenv-run *args:
  {{ testenv_env }} cargo run -- {{ args }}

testenv-clean:
  rm -rf {{ testenv_dir }}
