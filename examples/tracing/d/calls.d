/*
 * Calls: latency per target instance, and who calls whom.
 *
 *   dtrace -q -p <node pid> -s examples/tracing/d/calls.d
 *
 * call-start(call, from, to, bytes); call-done(call, to, outcome).
 * Instance ids are the middle number of a pid (0.<id>.<incarnation>);
 * a sender of 0 is the admin API.
 */

bulkhead$target:::call-start
{
	start[arg0] = timestamp;
	@calls[arg1, arg2] = count();
}

bulkhead$target:::call-done
/start[arg0]/
{
	@latency_us[arg1] = quantize((timestamp - start[arg0]) / 1000);
	start[arg0] = 0;
}

tick-5s
{
	printf("\n--- calls (from, to) in the last 5 s ---\n");
	printa("%10d %10d %@8d\n", @calls);
	trunc(@calls);
}

END
{
	trunc(@calls);
	printf("\n--- call latency (us) by target instance id ---\n");
}
