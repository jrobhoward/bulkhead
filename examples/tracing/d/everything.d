/*
 * Every probe, one line each. Busy nodes print a lot.
 *
 *   dtrace -q -p <node pid> -s examples/tracing/d/everything.d
 */

bulkhead$target:::call-start { printf("call-start         call=%d from=%d to=%d bytes=%d\n", arg0, arg1, arg2, arg3); }
bulkhead$target:::call-done { printf("call-done          call=%d to=%d %s\n", arg0, arg1, copyinstr(arg2, arg3)); }
bulkhead$target:::cast { printf("cast               from=%d to=%d bytes=%d %s\n", arg0, arg1, arg2, copyinstr(arg3, arg4)); }
bulkhead$target:::cast-wait { printf("cast-wait          from=%d to=%d waited_ns=%d %s\n", arg0, arg1, arg2, copyinstr(arg3, arg4)); }
bulkhead$target:::callback-start { printf("callback-start     pid=%d %s\n", arg0, copyinstr(arg1, arg2)); }
bulkhead$target:::callback-done { printf("callback-done      pid=%d %s guest_ns=%d ok=%d\n", arg0, copyinstr(arg1, arg2), arg3, arg4); }
bulkhead$target:::instance-exit { printf("instance-exit      0.%d.%d %s\n", arg0, arg1, copyinstr(arg2, arg3)); }
bulkhead$target:::supervisor-restart { printf("supervisor-restart 0.%d.%d %s\n", arg0, arg1, copyinstr(arg2, arg3)); }
bulkhead$target:::upgrade-start { printf("upgrade-start      %s -> %s\n", copyinstr(arg0, arg1), copyinstr(arg2, arg3)); }
bulkhead$target:::upgrade-done { printf("upgrade-done       %s -> %s ok=%d\n", copyinstr(arg0, arg1), copyinstr(arg2, arg3), arg4); }
bulkhead$target:::mailbox-drop { printf("mailbox-drop       pid=%d %s\n", arg0, copyinstr(arg1, arg2)); }
bulkhead$target:::node-down { printf("node-down          node=%d session=%d\n", arg0, arg1); }
