fn respond(ctx) { let n = ctx.state.incr("n"); http(200, `n=${n}`) }
