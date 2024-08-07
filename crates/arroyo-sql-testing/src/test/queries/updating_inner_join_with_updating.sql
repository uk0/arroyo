--fail=Error during planning: can't handle updating right side of join
CREATE TABLE impulse (
      timestamp TIMESTAMP,
      counter bigint unsigned not null,
      subtask_index bigint unsigned not null
    ) WITH (
      connector = 'single_file',
      path = '$input_dir/impulse.json',
      format = 'json',
      type = 'source',
      event_time_field = 'timestamp'
    );


    CREATE TABLE output (
      left_counter bigint,
      counter_mod_2 bigint,
      right_count bigint
    ) WITH (
      connector = 'single_file',
      path = '$output_path',
      format = 'debezium_json',
      type = 'sink'
    );

INSERT INTO output
select counter as left_counter, counter_mod_2, right_count from impulse inner join
     (select counter % 2 as counter_mod_2, cast(count(*) as bigint UNSIGNED) as right_count from impulse where counter < 3 group by 1)
    on counter = right_count where counter < 3;