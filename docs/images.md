# Optional image generation

Images are off by default. This feature uses a separately configured image
provider through native stdio MCP; it does not change your coding provider.

Supported provider request shapes are OpenAI Images and Qwen Messages. Choose
the endpoint, model and protocol from your provider's documentation.

## Enable

```sh
github-adapter image-enable --endpoint https://YOUR-PROVIDER-ENDPOINT --model YOUR-MODEL --protocol openai-images
```

The CLI prompts for the provider key with terminal echo disabled. For unattended
setup, use `--key-env NAME` to read an explicitly named environment variable.
The key is stored in the OS credential store and excluded from Codex settings.

The adapter registers an owned MCP entry and discovery skill for the selected
Codex profile. Existing user-edited entries are preserved.

## Check or repair

```sh
github-adapter image-status --check
github-adapter image-enable
```

The check performs MCP initialization and tool listing without generating an
image. Repair reuses the saved provider configuration. A client with a cached
tool/skill catalog can need one restart.

## Generate

```sh
github-adapter image-generate --prompt-file prompt.txt --output result.png
```

Use a new output path. `image-status` lists sizes supported by the configured
model; `--size auto` uses its square default. A failed or ambiguous image request
is not automatically repeated.

## Disable

```sh
github-adapter image-disable
```

Disable revokes the known image credentials and removes unchanged owned
registration. Conflicting user edits remain available for inspection.
