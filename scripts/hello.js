const info = rt.hostInfo();
rt.print(`hello from ${info.runtime} ${info.version} on ${info.engine}\n`);
rt.print("answer: " + (20 + 22) + "\n");
await rt.sleep(10);
rt.print("slept 10ms and returned\n");
