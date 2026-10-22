//! Several questions in flight at once, results in order.
//!
//! The first item runs alone, so its request writes the prompt cache that the
//! rest read (requests sent together before it exists all miss it); then up
//! to `n` run at a time. `each` sees every result in item order and may stop
//! the run: items not yet started are then skipped, and those already in
//! flight finish and are handed to `each` too, so nothing a request spent goes
//! unrecorded. One task drives them all, so nothing here needs `Send`.

use std::{cell::Cell, future::Future};

use futures::StreamExt as _;

/// What `each` wants after a result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// Keep going.
    Continue,
    /// Start nothing more.
    Stop,
}

/// Run `f` over `items`, the first alone and then up to `n` at a time,
/// handing each result to `each` in item order.
///
/// # Errors
/// The first error from `f` or `each`; items in flight are dropped then.
pub async fn in_order<I, T, Fut>(
    items: impl IntoIterator<Item = I>,
    n: usize,
    f: impl Fn(I) -> Fut,
    mut each: impl FnMut(T) -> anyhow::Result<Flow>,
) -> anyhow::Result<()>
where
    Fut: Future<Output = anyhow::Result<T>>,
{
    let mut items = items.into_iter();
    let Some(first) = items.next() else {
        return Ok(());
    };
    if each(f(first).await?)? == Flow::Stop {
        return Ok(());
    }
    let stop = Cell::new(false);
    let mut results = futures::stream::iter(items)
        .map(|item| {
            let (f, stop) = (&f, &stop);
            async move {
                if stop.get() {
                    return Ok(None);
                }
                f(item).await.map(Some)
            }
        })
        .buffered(n.max(1));
    while let Some(result) = results.next().await {
        if let Some(t) = result?
            && each(t)? == Flow::Stop
        {
            stop.set(true);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, time::Duration};

    use super::*;

    #[tokio::test]
    async fn results_come_in_order_and_a_stop_skips_what_has_not_started() -> anyhow::Result<()> {
        let started = RefCell::new(Vec::new());
        let mut seen = Vec::new();
        in_order(
            0..10u64,
            3,
            |i| {
                started.borrow_mut().push(i);
                async move {
                    // Later items finish first: order must not follow completion.
                    tokio::time::sleep(Duration::from_millis(30 - 3 * (i % 4))).await;
                    Ok(i)
                }
            },
            |i| {
                seen.push(i);
                Ok(if i == 4 { Flow::Stop } else { Flow::Continue })
            },
        )
        .await?;
        // Everything started is reported, in order, and nothing past the window.
        let started = started.into_inner();
        assert_eq!(seen, started);
        assert!(seen.starts_with(&[0, 1, 2, 3, 4]));
        assert!(seen.len() <= 4 + 3, "{seen:?}");
        Ok(())
    }

    #[tokio::test]
    async fn the_first_item_runs_alone() -> anyhow::Result<()> {
        let live = Cell::new(0usize);
        let most = Cell::new(0usize);
        let first_alone = Cell::new(true);
        in_order(
            0..6u32,
            4,
            |i| {
                let (live, most, first_alone) = (&live, &most, &first_alone);
                async move {
                    live.set(live.get() + 1);
                    most.set(most.get().max(live.get()));
                    if i == 0 && live.get() > 1 {
                        first_alone.set(false);
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    live.set(live.get() - 1);
                    Ok(())
                }
            },
            |()| Ok(Flow::Continue),
        )
        .await?;
        assert!(first_alone.get());
        assert!(most.get() > 1 && most.get() <= 4, "{}", most.get());
        Ok(())
    }
}
