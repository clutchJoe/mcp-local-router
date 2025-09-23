#!/usr/bin/env node

const readline = require('node:readline');
const { stdin, stdout, stderr } = process;

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--act') {
      const value = argv[i + 1];
      if (!value || value.startsWith('--')) {
        throw new Error('Missing value for --act');
      }
      args.act = value;
      i += 1;
    }
  }
  return args;
}

let actName;
try {
  const parsed = parseArgs(process.argv.slice(2));
  actName = parsed.act || 'actor';
} catch (error) {
  stderr.write(`${error.message}\n`);
  process.exit(1);
}

const toolName = `ask_${actName}`;
const implementation = {
  name: 'mock-mcp',
  version: '0.1.0',
};

const quotes = [
  "To be, or not to be: that is the question.",
  "All the world's a stage, and all the men and women merely players.",
  "The lady doth protest too much, methinks.",
  "Cowards die many times before their deaths; the valiant never taste of death but once.",
  "Some are born great, some achieve greatness, and some have greatness thrust upon them.",
  "If music be the food of love, play on.",
  "Brevity is the soul of wit.",
  "The course of true love never did run smooth.",
  "A horse! a horse! my kingdom for a horse!",
  "We are such stuff as dreams are made on; and our little life is rounded with a sleep."
];

function randomQuote() {
  const index = Math.floor(Math.random() * quotes.length);
  return quotes[index];
}

function sendMessage(message) {
  stdout.write(`${JSON.stringify(message)}\n`);
}

function sendError(id, code, message, data) {
  const error = { code, message };
  if (data !== undefined) {
    error.data = data;
  }
  sendMessage({ jsonrpc: '2.0', id: id ?? null, error });
}

function handleInitialize(id) {
  sendMessage({
    jsonrpc: '2.0',
    id,
    result: {
      protocolVersion: '2024-11-05',
      capabilities: {
        tools: {},
      },
      serverInfo: implementation,
      instructions: `Use ${toolName} to receive a random Shakespeare quote.`,
    },
  });
}

function handleListTools(id) {
  sendMessage({
    jsonrpc: '2.0',
    id,
    result: {
      tools: [
        {
          name: toolName,
          description: `Ask ${actName} for a random Shakespeare quote.`,
          inputSchema: {
            type: 'object',
            properties: {
              prompt: {
                type: 'string',
                description: 'Optional hint for the performer.',
              },
            },
            additionalProperties: false,
          },
        },
      ],
    },
  });
}

function handleCallTool(id, params) {
  if (!params || !params.name) {
    sendError(id, -32602, 'Missing tool name');
    return;
  }
  if (params.name !== toolName) {
    sendError(id, -32601, `Unknown tool: ${params.name}`);
    return;
  }
  sendMessage({
    jsonrpc: '2.0',
    id,
    result: {
      content: [
        {
          type: 'text',
          text: randomQuote(),
        },
      ],
      isError: false,
    },
  });
}

function handlePing(id) {
  sendMessage({ jsonrpc: '2.0', id, result: {} });
}

function handleRequest(message) {
  const { id, method, params } = message;
  switch (method) {
    case 'initialize':
      handleInitialize(id);
      break;
    case 'tools/list':
      handleListTools(id);
      break;
    case 'tools/call':
      handleCallTool(id, params);
      break;
    case 'ping':
      handlePing(id);
      break;
    default:
      sendError(id, -32601, `Method not found: ${method}`);
      break;
  }
}

const rl = readline.createInterface({ input: stdin });

rl.on('line', (line) => {
  const trimmed = line.trim();
  if (!trimmed) {
    return;
  }
  let message;
  try {
    message = JSON.parse(trimmed);
  } catch (error) {
    sendError(null, -32700, 'Parse error', { detail: error.message });
    return;
  }

  if (typeof message !== 'object' || message === null) {
    sendError(null, -32600, 'Invalid request');
    return;
  }

  if (Object.prototype.hasOwnProperty.call(message, 'id') && Object.prototype.hasOwnProperty.call(message, 'method')) {
    handleRequest(message);
    return;
  }

  if (Object.prototype.hasOwnProperty.call(message, 'method')) {
    // Notification: ignore.
    return;
  }

  // Unexpected message type: ignore silently.
});

rl.on('close', () => {
  process.exit(0);
});
