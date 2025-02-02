use crate::model::cfd::{Cfd, CfdState, Order, OrderId};
use crate::model::{BitMexPriceEventId, Usd};
use anyhow::{Context, Result};
use rust_decimal::Decimal;
use sqlx::pool::PoolConnection;
use sqlx::{Sqlite, SqlitePool};
use std::mem;
use std::str::FromStr;
use time::Duration;

pub async fn run_migrations(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}

pub async fn insert_order(order: &Order, conn: &mut PoolConnection<Sqlite>) -> anyhow::Result<()> {
    let query_result = sqlx::query(
        r#"insert into orders (
            uuid,
            trading_pair,
            position,
            initial_price,
            min_quantity,
            max_quantity,
            leverage,
            liquidation_price,
            creation_timestamp_seconds,
            settlement_time_interval_seconds,
            origin,
            oracle_event_id
        ) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)"#,
    )
    .bind(&order.id)
    .bind(&order.trading_pair)
    .bind(&order.position)
    .bind(&order.price.to_string())
    .bind(&order.min_quantity.to_string())
    .bind(&order.max_quantity.to_string())
    .bind(order.leverage.get())
    .bind(&order.liquidation_price.to_string())
    .bind(&order.creation_timestamp.seconds())
    .bind(&order.settlement_time_interval_hours.whole_seconds())
    .bind(&order.origin)
    .bind(&order.oracle_event_id.to_string())
    .execute(conn)
    .await?;

    if query_result.rows_affected() != 1 {
        anyhow::bail!("failed to insert order");
    }

    Ok(())
}

pub async fn load_order_by_id(
    id: OrderId,
    conn: &mut PoolConnection<Sqlite>,
) -> anyhow::Result<Order> {
    let row = sqlx::query!(
        r#"
        select
            uuid as "uuid: crate::model::cfd::OrderId",
            trading_pair as "trading_pair: crate::model::TradingPair",
            position as "position: crate::model::Position",
            initial_price,
            min_quantity,
            max_quantity,
            leverage as "leverage: crate::model::Leverage",
            liquidation_price,
            creation_timestamp_seconds as "ts_secs: crate::model::Timestamp",
            settlement_time_interval_seconds as "settlement_time_interval_secs: i64",
            origin as "origin: crate::model::cfd::Origin",
            oracle_event_id

        from orders
        where uuid = $1
        "#,
        id
    )
    .fetch_one(conn)
    .await?;

    Ok(Order {
        id: row.uuid,
        trading_pair: row.trading_pair,
        position: row.position,
        price: row.initial_price.parse()?,
        min_quantity: row.min_quantity.parse()?,
        max_quantity: row.max_quantity.parse()?,
        leverage: row.leverage,
        liquidation_price: row.liquidation_price.parse()?,
        creation_timestamp: row.ts_secs,
        settlement_time_interval_hours: Duration::new(row.settlement_time_interval_secs, 0),
        origin: row.origin,
        oracle_event_id: row.oracle_event_id.parse()?,
    })
}

pub async fn insert_cfd(cfd: &Cfd, conn: &mut PoolConnection<Sqlite>) -> anyhow::Result<()> {
    let state = serde_json::to_string(&cfd.state)?;
    let query_result = sqlx::query(
        r#"
        insert into cfds (
            order_id,
            order_uuid,
            quantity_usd
        )
        select
            id as order_id,
            uuid as order_uuid,
            $2 as quantity_usd
        from orders
        where uuid = $1;

        insert into cfd_states (
            cfd_id,
            state
        )
        select
            id as cfd_id,
            $3 as state
        from cfds
        order by id desc limit 1;
        "#,
    )
    .bind(&cfd.order.id)
    .bind(&cfd.quantity_usd.to_string())
    .bind(state)
    .execute(conn)
    .await?;

    // Should be 2 because we insert into cfds and cfd_states
    if query_result.rows_affected() != 2 {
        anyhow::bail!("failed to insert cfd");
    }

    Ok(())
}

pub async fn append_cfd_state(cfd: &Cfd, conn: &mut PoolConnection<Sqlite>) -> anyhow::Result<()> {
    let cfd_id = load_cfd_id_by_order_uuid(cfd.order.id, conn).await?;
    let current_state = load_latest_cfd_state(cfd_id, conn)
        .await
        .context("loading latest state failed")?;
    let new_state = &cfd.state;

    if mem::discriminant(&current_state) == mem::discriminant(new_state) {
        // Since we have states where we add information this happens quite frequently
        tracing::trace!(
            "Same state transition for cfd with order_id {}: {}",
            cfd.order.id,
            current_state
        );
    }

    let cfd_state = serde_json::to_string(new_state)?;

    sqlx::query(
        r#"
        insert into cfd_states (
            cfd_id,
            state
        ) values ($1, $2);
        "#,
    )
    .bind(cfd_id)
    .bind(cfd_state)
    .execute(conn)
    .await?;

    Ok(())
}

async fn load_cfd_id_by_order_uuid(
    order_uuid: OrderId,
    conn: &mut PoolConnection<Sqlite>,
) -> anyhow::Result<i64> {
    let cfd_id = sqlx::query!(
        r#"
        select
            id
        from cfds
        where order_uuid = $1;
        "#,
        order_uuid
    )
    .fetch_one(conn)
    .await?;

    let cfd_id = cfd_id.id.context("No cfd found")?;

    Ok(cfd_id)
}

async fn load_latest_cfd_state(
    cfd_id: i64,
    conn: &mut PoolConnection<Sqlite>,
) -> anyhow::Result<CfdState> {
    let latest_cfd_state = sqlx::query!(
        r#"
        select
            state
        from cfd_states
        where cfd_id = $1
        order by id desc
        limit 1;
        "#,
        cfd_id
    )
    .fetch_one(conn)
    .await?;

    let latest_cfd_state_in_db: CfdState = serde_json::from_str(latest_cfd_state.state.as_str())?;

    Ok(latest_cfd_state_in_db)
}

pub async fn load_cfd_by_order_id(
    order_id: OrderId,
    conn: &mut PoolConnection<Sqlite>,
) -> Result<Cfd> {
    let row = sqlx::query!(
        r#"
        with ord as (
            select
                id as order_id,
                uuid,
                trading_pair,
                position,
                initial_price,
                min_quantity,
                max_quantity,
                leverage,
                liquidation_price,
                creation_timestamp_seconds as ts_secs,
                settlement_time_interval_seconds as settlement_time_interval_secs,
                origin,
                oracle_event_id
            from orders
        ),

        cfd as (
            select
                ord.order_id,
                id as cfd_id,
                quantity_usd
            from cfds
                inner join ord on ord.order_id = cfds.order_id
        ),

        state as (
            select
                id as state_id,
                cfd.order_id,
                cfd.quantity_usd,
                state
            from cfd_states
                inner join cfd on cfd.cfd_id = cfd_states.cfd_id
            where id in (
                select
                    max(id) as id
                from cfd_states
                group by (cfd_id)
            )
        )

        select
            ord.uuid as "uuid: crate::model::cfd::OrderId",
            ord.trading_pair as "trading_pair: crate::model::TradingPair",
            ord.position as "position: crate::model::Position",
            ord.initial_price,
            ord.min_quantity,
            ord.max_quantity,
            ord.leverage as "leverage: crate::model::Leverage",
            ord.liquidation_price,
            ord.ts_secs as "ts_secs: crate::model::Timestamp",
            ord.settlement_time_interval_secs as "settlement_time_interval_secs: i64",
            ord.origin as "origin: crate::model::cfd::Origin",
            ord.oracle_event_id,
            state.quantity_usd,
            state.state

        from ord
            inner join state on state.order_id = ord.order_id

        where ord.uuid = $1
        "#,
        order_id
    )
    .fetch_one(conn)
    .await?;

    let order = Order {
        id: row.uuid,
        trading_pair: row.trading_pair,
        position: row.position,
        price: row.initial_price.parse()?,
        min_quantity: row.min_quantity.parse()?,
        max_quantity: row.max_quantity.parse()?,
        leverage: row.leverage,
        liquidation_price: row.liquidation_price.parse()?,
        creation_timestamp: row.ts_secs,
        settlement_time_interval_hours: Duration::new(row.settlement_time_interval_secs, 0),
        origin: row.origin,
        oracle_event_id: row.oracle_event_id.parse()?,
    };

    // TODO:
    // still have the use of serde_json::from_str() here, which will be dealt with
    // via https://github.com/comit-network/hermes/issues/290
    Ok(Cfd {
        order,
        quantity_usd: Usd::new(Decimal::from_str(&row.quantity_usd)?),
        state: serde_json::from_str(row.state.as_str())?,
    })
}

/// Loads all CFDs with the latest state as the CFD state
pub async fn load_all_cfds(conn: &mut PoolConnection<Sqlite>) -> anyhow::Result<Vec<Cfd>> {
    let rows = sqlx::query!(
        r#"
        with ord as (
            select
                id as order_id,
                uuid,
                trading_pair,
                position,
                initial_price,
                min_quantity,
                max_quantity,
                leverage,
                liquidation_price,
                creation_timestamp_seconds as ts_secs,
                settlement_time_interval_seconds as settlement_time_interval_secs,
                origin,
                oracle_event_id
            from orders
        ),

        cfd as (
            select
                ord.order_id,
                id as cfd_id,
                quantity_usd
            from cfds
                inner join ord on ord.order_id = cfds.order_id
        ),

        state as (
            select
                id as state_id,
                cfd.order_id,
                cfd.quantity_usd,
                state
            from cfd_states
                inner join cfd on cfd.cfd_id = cfd_states.cfd_id
            where id in (
                select
                    max(id) as id
                from cfd_states
                group by (cfd_id)
            )
        )

        select
            ord.uuid as "uuid: crate::model::cfd::OrderId",
            ord.trading_pair as "trading_pair: crate::model::TradingPair",
            ord.position as "position: crate::model::Position",
            ord.initial_price,
            ord.min_quantity,
            ord.max_quantity,
            ord.leverage as "leverage: crate::model::Leverage",
            ord.liquidation_price,
            ord.ts_secs as "ts_secs: crate::model::Timestamp",
            ord.settlement_time_interval_secs as "settlement_time_interval_secs: i64",
            ord.origin as "origin: crate::model::cfd::Origin",
            ord.oracle_event_id,
            state.quantity_usd,
            state.state

        from ord
            inner join state on state.order_id = ord.order_id
        "#
    )
    .fetch_all(conn)
    .await?;

    let cfds = rows
        .into_iter()
        .map(|row| {
            let order = Order {
                id: row.uuid,
                trading_pair: row.trading_pair,
                position: row.position,
                price: row.initial_price.parse()?,
                min_quantity: row.min_quantity.parse()?,
                max_quantity: row.max_quantity.parse()?,
                leverage: row.leverage,
                liquidation_price: row.liquidation_price.parse()?,
                creation_timestamp: row.ts_secs,
                settlement_time_interval_hours: Duration::new(row.settlement_time_interval_secs, 0),
                origin: row.origin,
                oracle_event_id: row.oracle_event_id.parse()?,
            };

            Ok(Cfd {
                order,
                quantity_usd: Usd::new(Decimal::from_str(&row.quantity_usd)?),
                state: serde_json::from_str(row.state.as_str())?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(cfds)
}

/// Loads all CFDs with the latest state as the CFD state
pub async fn load_cfds_by_oracle_event_id(
    oracle_event_id: BitMexPriceEventId,
    conn: &mut PoolConnection<Sqlite>,
) -> anyhow::Result<Vec<Cfd>> {
    let event_id = oracle_event_id.to_string();
    let rows = sqlx::query!(
        r#"
        with ord as (
            select
                id as order_id,
                uuid,
                trading_pair,
                position,
                initial_price,
                min_quantity,
                max_quantity,
                leverage,
                liquidation_price,
                creation_timestamp_seconds as ts_secs,
                settlement_time_interval_seconds as settlement_time_interval_secs,
                origin,
                oracle_event_id
            from orders
        ),

        cfd as (
            select
                ord.order_id,
                id as cfd_id,
                quantity_usd
            from cfds
                inner join ord on ord.order_id = cfds.order_id
        ),

        state as (
            select
                id as state_id,
                cfd.order_id,
                cfd.quantity_usd,
                state
            from cfd_states
                inner join cfd on cfd.cfd_id = cfd_states.cfd_id
            where id in (
                select
                    max(id) as id
                from cfd_states
                group by (cfd_id)
            )
        )

        select
            ord.uuid as "uuid: crate::model::cfd::OrderId",
            ord.trading_pair as "trading_pair: crate::model::TradingPair",
            ord.position as "position: crate::model::Position",
            ord.initial_price,
            ord.min_quantity,
            ord.max_quantity,
            ord.leverage as "leverage: crate::model::Leverage",
            ord.liquidation_price,
            ord.ts_secs as "ts_secs: crate::model::Timestamp",
            ord.settlement_time_interval_secs as "settlement_time_interval_secs: i64",
            ord.origin as "origin: crate::model::cfd::Origin",
            ord.oracle_event_id,
            state.quantity_usd,
            state.state

        from ord
            inner join state on state.order_id = ord.order_id

        where ord.oracle_event_id = $1
        "#,
        event_id
    )
    .fetch_all(conn)
    .await?;

    let cfds = rows
        .into_iter()
        .map(|row| {
            let order = Order {
                id: row.uuid,
                trading_pair: row.trading_pair,
                position: row.position,
                price: row.initial_price.parse()?,
                min_quantity: row.min_quantity.parse()?,
                max_quantity: row.max_quantity.parse()?,
                leverage: row.leverage,
                liquidation_price: row.liquidation_price.parse()?,
                creation_timestamp: row.ts_secs,
                settlement_time_interval_hours: Duration::new(row.settlement_time_interval_secs, 0),
                origin: row.origin,
                oracle_event_id: row.oracle_event_id.parse()?,
            };

            Ok(Cfd {
                order,
                quantity_usd: row.quantity_usd.parse()?,
                state: serde_json::from_str(row.state.as_str())?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(cfds)
}

#[cfg(test)]
mod tests {
    use crate::cfd_actors;
    use pretty_assertions::assert_eq;
    use rand::Rng;
    use rust_decimal_macros::dec;
    use sqlx::SqlitePool;
    use time::macros::datetime;
    use time::OffsetDateTime;
    use tokio::sync::watch;

    use crate::db::{self, insert_order};
    use crate::model::cfd::{Cfd, CfdState, Order, Origin};
    use crate::model::{Price, Usd};

    use super::*;

    #[tokio::test]
    async fn test_insert_and_load_order() {
        let mut conn = setup_test_db().await;

        let order = Order::dummy().insert(&mut conn).await;
        let loaded = load_order_by_id(order.id, &mut conn).await.unwrap();

        assert_eq!(order, loaded);
    }

    #[tokio::test]
    async fn test_insert_and_load_cfd() {
        let mut conn = setup_test_db().await;

        let cfd = Cfd::dummy().insert(&mut conn).await;
        let loaded = load_all_cfds(&mut conn).await.unwrap();

        assert_eq!(vec![cfd], loaded);
    }

    #[tokio::test]
    async fn test_insert_like_cfd_actor() {
        let mut conn = setup_test_db().await;

        let cfds = load_all_cfds(&mut conn).await.unwrap();

        let (cfd_feed_sender, cfd_feed_receiver) = watch::channel(cfds.clone());

        assert_eq!(cfd_feed_receiver.borrow().clone(), vec![]);

        let cfd_1 = Cfd::dummy();
        db::insert_order(&cfd_1.order, &mut conn).await.unwrap();
        cfd_actors::insert_cfd(&cfd_1, &mut conn, &cfd_feed_sender)
            .await
            .unwrap();

        assert_eq!(cfd_feed_receiver.borrow().clone(), vec![cfd_1.clone()]);

        let cfd_2 = Cfd::dummy();
        db::insert_order(&cfd_2.order, &mut conn).await.unwrap();
        cfd_actors::insert_cfd(&cfd_2, &mut conn, &cfd_feed_sender)
            .await
            .unwrap();
        assert_eq!(cfd_feed_receiver.borrow().clone(), vec![cfd_1, cfd_2]);
    }

    #[tokio::test]
    async fn test_insert_and_load_cfd_by_order_id() {
        let mut conn = setup_test_db().await;

        let cfd = Cfd::dummy().insert(&mut conn).await;
        let loaded = load_cfd_by_order_id(cfd.order.id, &mut conn).await.unwrap();

        assert_eq!(cfd, loaded)
    }

    #[tokio::test]
    async fn test_insert_and_load_cfd_by_order_id_multiple() {
        let mut conn = setup_test_db().await;

        let cfd1 = Cfd::dummy().insert(&mut conn).await;
        let cfd2 = Cfd::dummy().insert(&mut conn).await;

        let loaded_1 = load_cfd_by_order_id(cfd1.order.id, &mut conn)
            .await
            .unwrap();
        let loaded_2 = load_cfd_by_order_id(cfd2.order.id, &mut conn)
            .await
            .unwrap();

        assert_eq!(cfd1, loaded_1);
        assert_eq!(cfd2, loaded_2);
    }

    #[tokio::test]
    async fn test_insert_and_load_cfd_by_oracle_event_id() {
        let mut conn = setup_test_db().await;

        let cfd_1 = Cfd::dummy()
            .with_event_id(BitMexPriceEventId::event1())
            .insert(&mut conn)
            .await;
        let cfd_2 = Cfd::dummy()
            .with_event_id(BitMexPriceEventId::event1())
            .insert(&mut conn)
            .await;
        let cfd_3 = Cfd::dummy()
            .with_event_id(BitMexPriceEventId::event2())
            .insert(&mut conn)
            .await;

        let cfds_event_1 = load_cfds_by_oracle_event_id(BitMexPriceEventId::event1(), &mut conn)
            .await
            .unwrap();

        let cfds_event_2 = load_cfds_by_oracle_event_id(BitMexPriceEventId::event2(), &mut conn)
            .await
            .unwrap();

        assert_eq!(vec![cfd_1, cfd_2], cfds_event_1);
        assert_eq!(vec![cfd_3], cfds_event_2);
    }

    #[tokio::test]
    async fn test_insert_new_cfd_state_and_load_with_multiple_cfd() {
        let mut conn = setup_test_db().await;

        let mut cfd_1 = Cfd::dummy().insert(&mut conn).await;

        cfd_1.state = CfdState::accepted();
        append_cfd_state(&cfd_1, &mut conn).await.unwrap();

        let cfds_from_db = load_all_cfds(&mut conn).await.unwrap();
        assert_eq!(vec![cfd_1.clone()], cfds_from_db);

        let mut cfd_2 = Cfd::dummy().insert(&mut conn).await;

        let cfds_from_db = load_all_cfds(&mut conn).await.unwrap();
        assert_eq!(vec![cfd_1.clone(), cfd_2.clone()], cfds_from_db);

        cfd_2.state = CfdState::rejected();
        append_cfd_state(&cfd_2, &mut conn).await.unwrap();

        let cfds_from_db = load_all_cfds(&mut conn).await.unwrap();
        assert_eq!(vec![cfd_1, cfd_2], cfds_from_db);
    }

    #[tokio::test]
    async fn test_insert_order_without_cfd_associates_with_correct_cfd() {
        let mut conn = setup_test_db().await;

        // Insert an order without a CFD
        let _order_1 = Order::dummy().insert(&mut conn).await;

        // Insert a CFD (this also inserts an order)
        let cfd_1 = Cfd::dummy().insert(&mut conn).await;

        let all_cfds = load_all_cfds(&mut conn).await.unwrap();

        assert_eq!(all_cfds, vec![cfd_1]);
    }

    // test more data; test will add 100 cfds to the database, with each
    // having a random number of random updates. Final results are deterministic.
    #[tokio::test]
    async fn test_multiple_cfd_updates_per_cfd() {
        let mut conn = setup_test_db().await;

        for i in 0..100 {
            let mut cfd = Cfd::dummy()
                .with_event_id(BitMexPriceEventId::event1())
                .insert(&mut conn)
                .await;

            let n_updates = rand::thread_rng().gen_range(1, 30);

            for _ in 0..n_updates {
                cfd.state = random_simple_state();
                append_cfd_state(&cfd, &mut conn).await.unwrap();
            }

            // verify current state is correct
            let loaded_by_order_id = load_cfd_by_order_id(cfd.order.id, &mut conn).await.unwrap();
            assert_eq!(loaded_by_order_id, cfd);

            // load_cfds_by_oracle_event_id can return multiple CFDs
            let loaded_by_oracle_event_id =
                load_cfds_by_oracle_event_id(BitMexPriceEventId::event1(), &mut conn)
                    .await
                    .unwrap();
            assert_eq!(loaded_by_oracle_event_id.len(), i + 1);
        }

        // verify query returns only one state per CFD
        let data = load_all_cfds(&mut conn).await.unwrap();

        assert_eq!(data.len(), 100);
    }

    fn random_simple_state() -> CfdState {
        match rand::thread_rng().gen_range(0, 5) {
            0 => CfdState::outgoing_order_request(),
            1 => CfdState::accepted(),
            2 => CfdState::rejected(),
            3 => CfdState::contract_setup(),
            _ => CfdState::setup_failed(String::from("dummy failure")),
        }
    }

    async fn setup_test_db() -> PoolConnection<Sqlite> {
        let pool = SqlitePool::connect(":memory:").await.unwrap();

        run_migrations(&pool).await.unwrap();

        pool.acquire().await.unwrap()
    }

    impl BitMexPriceEventId {
        fn event1() -> Self {
            BitMexPriceEventId::with_20_digits(datetime!(2021-10-13 10:00:00).assume_utc())
        }

        fn event2() -> Self {
            BitMexPriceEventId::with_20_digits(datetime!(2021-10-25 18:00:00).assume_utc())
        }
    }

    impl Cfd {
        fn dummy() -> Self {
            Cfd::new(
                Order::dummy(),
                Usd::new(dec!(1000)),
                CfdState::outgoing_order_request(),
            )
        }

        /// Insert this [`Cfd`] into the database, returning the instance for further chaining.
        async fn insert(self, conn: &mut PoolConnection<Sqlite>) -> Self {
            insert_order(&self.order, conn).await.unwrap();
            insert_cfd(&self, conn).await.unwrap();

            self
        }

        fn with_event_id(mut self, id: BitMexPriceEventId) -> Self {
            self.order.oracle_event_id = id;
            self
        }
    }

    impl Order {
        fn dummy() -> Self {
            Order::new(
                Price::new(dec!(1000)).unwrap(),
                Usd::new(dec!(100)),
                Usd::new(dec!(1000)),
                Origin::Theirs,
                BitMexPriceEventId::with_20_digits(OffsetDateTime::now_utc()),
                time::Duration::hours(24),
            )
            .unwrap()
        }

        /// Insert this [`Order`] into the database, returning the instance for further chaining.
        async fn insert(self, conn: &mut PoolConnection<Sqlite>) -> Self {
            insert_order(&self, conn).await.unwrap();

            self
        }
    }
}
