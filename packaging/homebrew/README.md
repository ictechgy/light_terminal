# Homebrew formula staging

`lterm.rb` is the formula intended for `ictechgy/homebrew-tap`.

Publish flow once the tap repository exists:

```bash
cp packaging/homebrew/lterm.rb /path/to/homebrew-tap/Formula/lterm.rb
cd /path/to/homebrew-tap
brew audit --strict --online Formula/lterm.rb
brew install --build-from-source Formula/lterm.rb
brew test lterm
git add Formula/lterm.rb
git commit -m "Add lterm formula"
git push origin main
```

The v0.1.0 source archive SHA-256 is:

```text
e1b8b663e89dae3f70f96f444e93851dd8031c4e6b78039808ea94b62b810485
```
