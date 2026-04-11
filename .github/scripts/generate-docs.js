/**
 * generate-docs.js
 *
 * Reads DOCS_IMPACT.yaml entries, gathers context (source code, current docs,
 * style guide), builds a structured prompt, calls AWS Bedrock (Claude), and
 * writes the generated markdown files to an output directory.
 *
 * Usage:
 *   node generate-docs.js \
 *     --impact /tmp/docs_impact.yaml \
 *     --diff /tmp/docs_impact_diff.txt \
 *     --context /tmp/docs-context \
 *     --output /tmp/docs-output
 *
 * Environment:
 *   AWS_BEDROCK_BEARER_TOKEN - Bearer token for Bedrock API authentication
 *   AWS_REGION               - AWS region for Bedrock (default: us-east-1)
 *   BEDROCK_MODEL_ID         - Model to invoke (default: us.anthropic.claude-sonnet-4-20250514-v1:0)
 */

const https = require("https");
const fs = require("fs");
const path = require("path");

const SECTION_LIMITS = {
  styleGuide: 20_000,
  impactYaml: 30_000,
  diffText: 30_000,
  sourceCode: 180_000,
  currentDocs: 120_000,
  gitLog: 10_000,
};

function truncateSection(label, content, maxChars) {
  if (!content || content.length <= maxChars) {
    return content || "";
  }

  const omitted = content.length - maxChars;
  return (
    content.slice(0, maxChars) +
    `\n\n[${label} truncated: omitted ${omitted} characters to stay within model input limits]\n`
  );
}

// ── Parse CLI args ──────────────────────────────────────────────
function parseArgs() {
  const args = process.argv.slice(2);
  const parsed = {};
  for (let i = 0; i < args.length; i += 2) {
    const key = args[i].replace(/^--/, "");
    parsed[key] = args[i + 1];
  }
  return parsed;
}

// ── Read file safely ────────────────────────────────────────────
function readFile(filePath) {
  try {
    return fs.readFileSync(filePath, "utf-8");
  } catch {
    return "";
  }
}

// ── Build the LLM prompt ────────────────────────────────────────
function buildPrompt({ impactYaml, diffText, styleGuide, sourceCode, currentDocs, gitLog }) {
  const boundedStyleGuide = truncateSection(
    "Style guide",
    styleGuide,
    SECTION_LIMITS.styleGuide
  );
  const boundedImpactYaml = truncateSection(
    "DOCS_IMPACT.yaml",
    impactYaml,
    SECTION_LIMITS.impactYaml
  );
  const boundedDiffText = truncateSection(
    "DOCS_IMPACT diff",
    diffText,
    SECTION_LIMITS.diffText
  );
  const boundedSourceCode = truncateSection(
    "Relevant source code",
    sourceCode,
    SECTION_LIMITS.sourceCode
  );
  const boundedCurrentDocs = truncateSection(
    "Current documentation",
    currentDocs,
    SECTION_LIMITS.currentDocs
  );
  const boundedGitLog = truncateSection(
    "Recent git history",
    gitLog,
    SECTION_LIMITS.gitLog
  );

  return `You are a technical writer for Selu, a personal AI agent platform.
Selu makes AI agents accessible to non-technical users. The product feels warm,
approachable, and simple — never like a developer tool.

## Your Task

Update or create documentation based on the code changes described below.
You will receive:
1. The DOCS_IMPACT.yaml entries describing what changed
2. The actual diff showing what was added
3. The relevant source code
4. The current documentation (if it exists — may be empty for new pages)
5. A style guide for writing conventions

## Style Guide

${boundedStyleGuide || "No style guide available. Use clear, warm, accessible language. Use Starlight-compatible MDX with frontmatter."}

## DOCS_IMPACT.yaml (full file)

\`\`\`yaml
${boundedImpactYaml}
\`\`\`

## Diff (new entries only)

\`\`\`diff
${boundedDiffText}
\`\`\`

## Relevant Source Code

${boundedSourceCode || "No source code available."}

## Current Documentation (to update)

${boundedCurrentDocs || "No existing documentation found for the affected sections. Create new pages."}

## Recent Git History

${boundedGitLog || "No git history available."}

## Output Rules

1. Generate BOTH English (en/) and German (de/) versions of every file.
2. Use Starlight-compatible MDX format with YAML frontmatter:
   \`\`\`mdx
   ---
   title: "Page Title"
   description: "Brief description for SEO and sidebar"
   ---

   Content here...
   \`\`\`
3. Use Starlight admonitions where appropriate:
   - \`:::note\` for helpful information
   - \`:::tip\` for best practices
   - \`:::caution\` for warnings
   - \`:::danger\` for critical warnings
4. Include code examples where they help understanding.
5. For user-guide content: use plain language, avoid jargon, explain concepts.
6. For developer-guide content: be precise and technical, include full code examples.
7. If updating an existing page, return the COMPLETE updated file (not just the diff).
8. If creating a new page, include appropriate frontmatter and structure.

## Output Format

Return each file in this exact format (the parser depends on it):

--- FILE: en/path/to/file.mdx ---
{complete file content}
--- END FILE ---

--- FILE: de/path/to/file.mdx ---
{complete file content}
--- END FILE ---

Generate all necessary files now.`;
}

// ── Parse LLM output into files ─────────────────────────────────
function parseOutput(text) {
  const files = [];
  const regex = /--- FILE: (.+?) ---\n([\s\S]*?)--- END FILE ---/g;
  let match;

  while ((match = regex.exec(text)) !== null) {
    files.push({
      path: match[1].trim(),
      content: match[2].trim() + "\n",
    });
  }

  return files;
}

// ── Write output files ──────────────────────────────────────────
function writeOutputFiles(outputDir, files) {
  for (const file of files) {
    const fullPath = path.join(outputDir, file.path);
    const dir = path.dirname(fullPath);
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(fullPath, file.content, "utf-8");
    console.log(`  Written: ${file.path}`);
  }
}

// ── Call Bedrock via HTTPS with bearer token ────────────────────
async function callBedrock(prompt) {
  const token = process.env.AWS_BEDROCK_BEARER_TOKEN;
  if (!token) {
    throw new Error(
      "AWS_BEDROCK_BEARER_TOKEN is not set. " +
        "Add it as a repository secret in GitHub Actions."
    );
  }

  const region = process.env.AWS_REGION || "us-east-1";
  const modelId =
    process.env.BEDROCK_MODEL_ID || "us.anthropic.claude-sonnet-4-20250514-v1:0";

  const payload = JSON.stringify({
    anthropic_version: "bedrock-2023-05-31",
    max_tokens: 16000,
    messages: [
      {
        role: "user",
        content: prompt,
      },
    ],
  });

  const hostname = `bedrock-runtime.${region}.amazonaws.com`;
  const urlPath = `/model/${encodeURIComponent(modelId)}/invoke`;

  console.log(`Calling Bedrock model: ${modelId} in ${region}`);
  console.log(`  POST https://${hostname}${urlPath}`);

  const responseBody = await new Promise((resolve, reject) => {
    const req = https.request(
      {
        hostname,
        port: 443,
        path: urlPath,
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Accept: "application/json",
          Authorization: `Bearer ${token}`,
          "Content-Length": Buffer.byteLength(payload),
        },
      },
      (res) => {
        const chunks = [];
        res.on("data", (chunk) => chunks.push(chunk));
        res.on("end", () => {
          const body = Buffer.concat(chunks).toString("utf-8");
          if (res.statusCode < 200 || res.statusCode >= 300) {
            reject(
              new Error(
                `Bedrock returned HTTP ${res.statusCode}: ${body.substring(0, 500)}`
              )
            );
            return;
          }
          try {
            resolve(JSON.parse(body));
          } catch (e) {
            reject(new Error(`Failed to parse Bedrock response: ${e.message}`));
          }
        });
      }
    );

    req.on("error", reject);
    req.write(payload);
    req.end();
  });

  if (!responseBody.content || responseBody.content.length === 0) {
    throw new Error("Empty response from Bedrock");
  }

  // Extract text from the response content blocks
  const text = responseBody.content
    .filter((block) => block.type === "text")
    .map((block) => block.text)
    .join("\n");

  console.log(`Response received: ${text.length} characters`);
  console.log(
    `Tokens used: input=${responseBody.usage?.input_tokens || "?"}, output=${responseBody.usage?.output_tokens || "?"}`
  );

  return text;
}

// ── Main ─────────────────────────────────────────────────────────
async function main() {
  const args = parseArgs();

  if (!args.impact || !args.diff || !args.context || !args.output) {
    console.error(
      "Usage: node generate-docs.js --impact <path> --diff <path> --context <dir> --output <dir>"
    );
    process.exit(1);
  }

  // Read inputs
  const impactYaml = readFile(args.impact);
  const diffText = readFile(args.diff);
  const styleGuide = readFile(path.join(args.context, "style_guide.md"));
  const sourceCode = readFile(path.join(args.context, "source_code.txt"));
  const currentDocs = readFile(path.join(args.context, "current_docs.txt"));
  const gitLog = readFile(path.join(args.context, "git_log.txt"));

  console.log("Prompt inputs:");
  console.log(`  style_guide.md: ${styleGuide.length} characters`);
  console.log(`  source_code.txt: ${sourceCode.length} characters`);
  console.log(`  current_docs.txt: ${currentDocs.length} characters`);
  console.log(`  git_log.txt: ${gitLog.length} characters`);
  console.log(`  DOCS_IMPACT.yaml: ${impactYaml.length} characters`);
  console.log(`  DOCS_IMPACT diff: ${diffText.length} characters`);

  // Check if there are actual entries to process
  if (!diffText.trim()) {
    console.log("No diff content. Nothing to generate.");
    return;
  }

  // Build prompt
  const prompt = buildPrompt({
    impactYaml,
    diffText,
    styleGuide,
    sourceCode,
    currentDocs,
    gitLog,
  });

  console.log(`Prompt built: ${prompt.length} characters`);

  // Call LLM
  const response = await callBedrock(prompt);

  // Parse response into files
  const files = parseOutput(response);

  if (files.length === 0) {
    console.log(
      "No files parsed from LLM response. The model may not have generated any docs."
    );
    console.log("Raw response (first 500 chars):", response.substring(0, 500));
    return;
  }

  console.log(`\nGenerated ${files.length} file(s):`);

  // Write files
  fs.mkdirSync(args.output, { recursive: true });
  writeOutputFiles(args.output, files);

  console.log("\nDone.");
}

main().catch((err) => {
  console.error("Error:", err.message);
  process.exit(1);
});
