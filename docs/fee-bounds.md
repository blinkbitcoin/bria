# Fee bounds

Bria takes its fee rates from external HTTP estimators (mempool.space, with
blockstream.info as a fallback). Those rates decide how much of a wallet's
balance leaves as miner fees on every batch, so the daemon bounds them before
they are used.

## Configuration

```yaml
app:
  fees:
    min_sat_per_vb: 1.0        # default: readings below this are raised to it
    max_sat_per_vb: 1000.0     # default: readings above this are refused
    on_max_exceeded: reject    # reject (default) | clamp
```

The daemon refuses to start if `min_sat_per_vb` or `max_sat_per_vb` is not a
finite positive number, if the minimum is above the maximum, or if the maximum
is above the hard ceiling described below.

## What happens to a reading

A reading is checked twice: once where it enters the process, and once where it
is handed to a consumer.

**At the provider boundary** a reading that is not a usable fee rate at all -
not finite, zero or negative, above a hard, non-configurable ceiling of
`10_000` sat/vB, or too small to represent - is rejected outright. The request
then falls through the normal chain: mempool.space, then blockstream.info, then
a cached value if `enable_stale_on_error` allows it, then an error. A rejected
reading is never cached, so it cannot outlive the provider recovering.

The hard ceiling is not configurable because no market condition produces it:
the most extreme organic fee events have peaked around 1_000 sat/vB and only
briefly. `max_sat_per_vb` is the knob for operator policy.

**At the consumer** the configured bounds apply, including to a value served
from the cache:

| Path | Below `min_sat_per_vb` | Above `max_sat_per_vb` |
|---|---|---|
| building a batch (`process_payout_queue`) | raised to the minimum | refused: the job errors and the batch is retried on the next queue tick (or clamped, with `on_max_exceeded: clamp`) |
| fee quotes (`EstimatePayoutFee`) | raised to the minimum | refused, so a quote cannot promise a rate the batcher will not use |
| fee encumbrance during a wallet sync | raised to the minimum | clamped to the maximum: no funds move and syncing must not stop |

`reject` is the default on the spending paths because a batch built at a
clamped-down rate is signed and broadcast within seconds, cannot be cancelled
afterwards, and holds its inputs reserved until it confirms - so clamping during
a genuine spike can block a queue for longer than deferring the batch would.
Payouts are never dropped: they stay unbatched and are picked up on the next
tick.

## Alerting

Both checks emit structured tracing events. The fields are stable; alert on
them.

| Event | Target | Level | Fields |
|---|---|---|---|
| `rejected implausible fee estimate` | `bria::fees::bounds` | WARN | `provider`, `priority`, `value`, `hard_max_sat_per_vb` |
| `fee estimate above configured maximum` | `bria::fees::bounds` | ERROR | `priority`, `observed_sat_per_vb`, `max_sat_per_vb`, `action` (`reject` or `clamp`) |
| `fee estimate below configured minimum` | `bria::fees::bounds` | WARN | `priority`, `observed_sat_per_vb`, `min_sat_per_vb`, `action` (`floor`) |
| `invalid fee bound config value; using the default` | `bria::fees::bounds` | WARN | `field`, `configured`, `default` |

A refused reading also surfaces on the `job.process_payout_queue` span as a
`FeeRateAboveMaximum` error, and that span records `fee_rate_sat_per_vb` for
every batch it does build.

## When an alert fires

`action = "reject"` means no batch is being built for that queue while the
reading stands. Check the estimators first: if the reading is genuine and the
payouts need to go out at that price, raise `max_sat_per_vb` or switch to
`on_max_exceeded: clamp`. If it is not genuine, the provider is wrong and the
default is doing its job.
