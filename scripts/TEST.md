#### Automated fresh/upgrade test flows (local + SSH)

If you want repeatable install testing on your Mac and a Raspberry Pi over SSH, use:

```bash
# Fresh isolated install on both hosts (safe: does not touch ~/.local/bin or ~/.oxydra)
./scripts/test-build-install.sh --mode fresh --tag "$OXYDRA_TAG" \
  --target local \
  --target ssh:pi@raspberrypi.local

# Normal upgrade on existing setups
./scripts/test-build-install.sh --mode upgrade --tag "$OXYDRA_TAG" \
  --target local \
  --target ssh:pi@raspberrypi.local

# Discard the fresh isolated install later (use the label printed by fresh mode)
./scripts/test-build-install.sh --mode fresh-clean --label <printed-label> \
  --target local \
  --target ssh:pi@raspberrypi.local
```

`scripts/.env` is auto-loaded when present (gitignored by default). The script writes those values into the fresh install as `runner.env` and generates `runner-with-env.sh`, which applies `runner.env` + `--config` automatically (and uses `--env-file runner.env` when present) so you can run any runner subcommand with the same env.

Example after a fresh run (replace `<label>`):
```bash
/tmp/oxydra-fresh-tests/<label>/runner-with-env.sh --user alice start
/tmp/oxydra-fresh-tests/<label>/runner-with-env.sh --user alice status
/tmp/oxydra-fresh-tests/<label>/runner-with-env.sh --user alice logs --tail 200
/tmp/oxydra-fresh-tests/<label>/runner-with-env.sh web --bind 127.0.0.1:9400
```
Use `--env-file /path/to/file` to use a different local env file, or `--no-env-file` to disable env loading.
