import fs from "node:fs";
import path from "node:path";

const ENV_SCRIPT = path.join(".claude", "hooks", "platform-env.sh");
const ENV_MARKER = "platform-env.sh";

/**
 * OpenCode 专用：在 tool.execute.before 直改 bash command，不经过 updatedInput，与用户 PreToolUse hook 解耦。
 */
export default async function PlatformEnvPlugin({ directory }) {
  const envFile = path.join(directory, ENV_SCRIPT);

  return {
    "tool.execute.before": async (input, output) => {
      if (input.tool !== "bash") {
        return;
      }

      const command = output.args?.command;
      if (typeof command !== "string" || !command.trim()) {
        return;
      }

      if (command.includes(ENV_MARKER)) {
        return;
      }

      if (!fs.existsSync(envFile)) {
        return;
      }

      const quotedEnvFile = `'${envFile.replace(/'/g, `'\\''`)}'`;
      output.args.command = `set -a && source ${quotedEnvFile} && set +a && ${command}`;
    },
  };
}
