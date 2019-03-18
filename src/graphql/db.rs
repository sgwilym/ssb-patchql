use crate::db::*;
use flumedb::offset_log::{LogEntry, OffsetLogIter};
use flumedb::BidirIterator;
use itertools::Itertools;
use serde_json::Value;
use std::collections::BTreeSet;

use crate::db::schema::authors::dsl::{author as author_row, authors as authors_table};
use crate::db::schema::keys::dsl::{id as keys_id_row, key as keys_key_row, keys as keys_table};
use crate::db::schema::links::dsl::{link_from_key_id, link_to_key_id, links as links_table};
use crate::db::schema::messages::dsl::{
    asserted_time, author_id, content, content_type, flume_seq as flume_seq_row, fork_key_id,
    is_decrypted, key_id, messages as messages_table, received_time, root_key_id, seq as seq_row,
};
use diesel::dsl::sql;
use diesel::prelude::*;
use diesel::result::Error;
use crate::lib::{SsbMessage, find_values_in_object_by_key};

#[derive(Default)]
pub struct DbMutation {}

#[derive(GraphQLObject)]
struct ProcessResults {
    chunk_size: i32,
    latest_sequence: Option<f64>,
}

struct LogIter<T> {
    log_iter: OffsetLogIter<T>,
}
impl<T> Iterator for LogIter<T> {
    type Item = LogEntry;

    fn next(&mut self) -> Option<Self::Item> {
        self.log_iter.next()
    }
}

graphql_object!(DbMutation: Context |&self| {
    field process(&executor, chunk_size = 100: i32) -> ProcessResults {
        //TODO: get the secret key from env
        let context = executor.context();
        let connection = context.connection.lock().unwrap();

        //We're using Max of flume_seq.
        //When the db is empty, we'll get None. 
        //When there is one item in the db, we'll get 0 (it's the first seq number you get)
        //When there's more than one you'll get some >0 number
        let max_seq = get_latest(&connection)
            .unwrap()
            .map(|val| val as u64);

        let log = context.log.lock().unwrap(); //block here until any other thread is done with the log.

        let num_to_skip: usize = match max_seq {
            None => 0,
            _ => 1
        };

        let starting_offset = max_seq.unwrap_or(0);
        let log_iter = LogIter{log_iter: log.iter_at_offset(starting_offset)};

        log_iter
            .skip(num_to_skip)
            .take(chunk_size as usize)
            .chunks(5000)
            .into_iter()
            .for_each(|chunk|{
                //We use iter tools to set an upper bound on the size of chunks we process here. 
                //It avoids collecting into a vec and consuming way too much memory if the caller
                //tries to process the entire log.
                let messages: Vec<(SsbMessage, u64)> = chunk
                    .map(|log_entry|{
                        let message: SsbMessage = serde_json::from_slice(&log_entry.data).unwrap();
                        (message, log_entry.offset)
                    })
                .collect();

                let mut keys_set = BTreeSet::new();
                let mut authors_set = BTreeSet::new();
                let mut links = Vec::new();

                messages.iter()
                    .for_each(|(message, _)|{
                        keys_set.insert(message.key.clone());
                        if let Value::String(root_key) = &message.value.content["root"]{
                            keys_set.insert(root_key.to_string());
                        }
                        if let Value::String(fork_key) = &message.value.content["fork"]{
                            keys_set.insert(fork_key.to_string());
                        }
                        authors_set.insert(message.value.author.clone());
                        find_values_in_object_by_key(&message.value.content, "link", &mut links);
                    });

                links.iter()
                    .filter(|link| link.is_string())
                    .map(|link| link.as_str().unwrap())
                    .filter(|link| link.starts_with('@'))
                    .for_each(|link|{
                        authors_set.insert(link.to_string());
                    });

                links.iter()
                    .filter(|link| link.is_string())
                    .map(|link| link.as_str().unwrap())
                    .filter(|link| link.starts_with('%'))
                    .for_each(|link|{
                        keys_set.insert(link.to_string());
                    });

                let keys_rows: Vec<_> = keys_set.iter()
                    .map(|key_string|{keys_key_row.eq(key_string)})
                    .collect();

                let authors_rows: Vec<_> = authors_set.iter()
                    .map(|author_string|{author_row.eq(author_string)})
                    .collect();

                connection.transaction::<_, Error, _>(||{

                    let messages_rows: Vec<_> = messages.iter()
                        .map(|(message, offset)|{
                            let key_id_query_string = format!("(SELECT id FROM keys WHERE key == '{}')", message.key); 

                            let root_key_id_query = match &message.value.content["root"] {
                                Value::String(root_key) => {
                                    let query_string = format!("(SELECT id FROM keys WHERE key == '{}')", root_key); 
                                    root_key_id.eq(sql(&query_string))
                                },
                                _ => root_key_id.eq(sql("NULL"))
                            };
                            let fork_key_id_query = match &message.value.content["fork"] {
                                Value::String(fork_key) => {
                                    let query_string = format!("(SELECT id FROM keys WHERE key == '{}')", fork_key); 
                                    fork_key_id.eq(sql(&query_string))
                                },
                                _ => fork_key_id.eq(sql("NULL"))
                            };

                            let author_id_query_string = format!("(SELECT id FROM authors WHERE author == '{}')", message.value.author); 

                            (
                                flume_seq_row.eq(*offset as i64),
                                key_id.eq(sql(&key_id_query_string)),
                                seq_row.eq(message.value.sequence as i32),
                                received_time.eq(message.timestamp),
                                asserted_time.eq(message.value.timestamp),
                                root_key_id_query,
                                fork_key_id_query,
                                author_id.eq(sql(&author_id_query_string)),
                                content_type.eq(message.value.content["type"].as_str()), 
                                content.eq(message.value.content.to_string()),
                                //TODO is_decrypted
                            )
                        })
                    .collect();

                    diesel::insert_or_ignore_into(authors_table)
                        .values(&authors_rows)
                        .execute(&(*connection))?;

                    diesel::insert_or_ignore_into(keys_table)
                        .values(&keys_rows)
                        .execute(&(*connection))?;

                    diesel::insert_into(messages_table)
                        .values(&messages_rows)
                        .execute(&(*connection))
                        .map_err(|err|{
                            println!("Nope nope nope: {}", err);
                            err
                        })?;

                    //TODO: links table
                    //TODO: votes table
                    //TODO: contacts table
                    //TODO: mentions table
                    //TODO: branches table

                    Ok(())
                }).unwrap();

            });

        let new_latest = get_latest(&connection).unwrap();
        ProcessResults{chunk_size, latest_sequence: new_latest}
    }
});

#[derive(Default)]
pub struct Db {}

graphql_object!(Db: Context |&self| {
    field latest(&executor) -> Option<f64> {
        //let id = self.id;
        let context = executor.context();
        let connection = context.connection.lock().unwrap();
        get_latest(&connection).unwrap()
    }
});
