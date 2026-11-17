/*
 * Guest execution time per callback, host time excluded, by instance id,
 * and callbacks that failed.
 *
 *   dtrace -q -p <node pid> -s examples/tracing/d/callbacks.d
 *
 * callback-done(pid, kind, guest_ns, ok); kind is a pointer and a length.
 */

bulkhead$target:::callback-done
{
	@guest_us[arg0] = quantize(arg3 / 1000);
}

bulkhead$target:::callback-done
/arg4 == 0/
{
	printf("failed callback: instance %d, %s, %d us of guest time\n", arg0, copyinstr(arg1, arg2), arg3 / 1000);
}

END
{
	printf("\n--- guest time per callback (us) by instance id ---\n");
}
