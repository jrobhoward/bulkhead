/*
 * Instance lifecycle as it happens: exits, restarts, rollouts and messages
 * dropped by full mailboxes.
 *
 *   dtrace -q -p <node pid> -s examples/tracing/d/lifecycle.d
 *
 * A &str argument is a pointer and a length; copyinstr(ptr, len) reads it.
 * DTrace shows `__` in a probe name as `-`.
 */

bulkhead$target:::instance-exit
{
	printf("%Y exit     0.%d.%d %s\n", walltimestamp, arg0, arg1, copyinstr(arg2, arg3));
}

bulkhead$target:::supervisor-restart
{
	printf("%Y restart  0.%d.%d (child %s)\n", walltimestamp, arg0, arg1, copyinstr(arg2, arg3));
}

bulkhead$target:::upgrade-start
{
	printf("%Y upgrade  %s -> %s\n", walltimestamp, copyinstr(arg0, arg1), copyinstr(arg2, arg3));
}

bulkhead$target:::upgrade-done
{
	printf("%Y upgraded %s -> %s ok=%d\n", walltimestamp, copyinstr(arg0, arg1), copyinstr(arg2, arg3), arg4);
}

bulkhead$target:::mailbox-drop
{
	@dropped_by_instance[arg0] = count();
}

tick-5s
{
	/* Prints nothing while no message was dropped. */
	printa(@dropped_by_instance);
	trunc(@dropped_by_instance);
}
