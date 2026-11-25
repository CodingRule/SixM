#!/usr/bin/env node
import { main } from '../src/client/main.js';

main().catch((err) => {
  console.error(err?.stack ?? err);
  process.exit(1);
});
