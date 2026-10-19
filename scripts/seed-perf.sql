-- Data for performance measurements (perf_big, perf_t1..perf_t1000). Idempotent.
create table if not exists perf_big as
select g as id, md5(g::text) as a, md5((g * 2)::text) as b, g % 1000 as c, now() - g * interval '1 second' as d,
       g::numeric / 7 as e, (g % 2 = 0) as f, repeat('x', 20) as h, gen_random_uuid() as i, jsonb_build_object('g', g) as j
from generate_series(1, 1000000) g;

do $$
begin
  for n in 1..1000 loop
    execute format('create table if not exists perf_t%s (id int primary key, name text)', n);
  end loop;
end $$;
