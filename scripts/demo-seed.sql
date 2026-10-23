-- Demo shop for README recordings: docker run -d --name rb-demo -e POSTGRES_HOST_AUTH_METHOD=trust -e POSTGRES_DB=demo -p 55433:5432 postgres:17
select setseed(0.42);
drop schema if exists shop cascade;
create schema shop;

create table shop.customers (
  id int primary key,
  name text not null,
  email text not null,
  country text not null,
  website text,
  created_at timestamptz not null
);
insert into shop.customers
select g,
       f || ' ' || l,
       lower(f || '.' || l || g) || '@example.com',
       (array['Brazil','Ireland','Portugal','Spain','Germany','United States','Canada','Japan'])[1 + (g * 7) % 8],
       case when g % 5 = 0 then 'https://example.com/' || lower(l) || g end,
       timestamptz '2024-01-01' + (g * interval '37 minutes')
from generate_series(1, 20000) g,
     lateral (select (array['Ana','Bruno','Carla','Diego','Eva','Felipe','Grace','Hugo','Iris','João','Kenji','Laura','Mateo','Nora','Owen','Paula'])[1 + g % 16] as f,
                     (array['Silva','Murphy','Costa','García','Müller','Smith','Tanaka','Oliveira','Kelly','Santos','Lopez','Schmidt'])[1 + (g / 16) % 12] as l) n;

create table shop.products (
  id int primary key,
  name text not null,
  category text not null,
  price numeric(10,2) not null,
  attributes jsonb not null
);
insert into shop.products
select g,
       c || ' ' || (array['Classic','Pro','Mini','Max','Lite','Plus'])[1 + g % 6] || ' ' || g,
       c,
       round((5 + random() * 495)::numeric, 2),
       jsonb_build_object(
         'color', (array['black','white','red','blue','green'])[1 + g % 5],
         'weight_g', 100 + (g * 37) % 2000,
         'tags', jsonb_build_array(lower(c), case when g % 3 = 0 then 'bestseller' else 'new' end),
         'dimensions', jsonb_build_object('w', 10 + g % 40, 'h', 5 + g % 30, 'd', 2 + g % 10))
from generate_series(1, 500) g,
     lateral (select (array['Headphones','Keyboard','Monitor','Mouse','Laptop','Speaker','Webcam','Charger'])[1 + g % 8] as c) k;

create table shop.orders (
  id int primary key,
  customer_id int not null references shop.customers,
  status text not null,
  ordered_at timestamptz not null,
  total numeric(12,2) not null default 0
);
insert into shop.orders
select g,
       1 + (random() * 19999)::int,
       (array['delivered','delivered','delivered','shipped','paid','pending','cancelled','refunded'])[1 + (random() * 7)::int],
       timestamptz '2025-01-01' + random() * interval '600 days',
       0
from generate_series(1, 200000) g;

create table shop.order_items (
  order_id int not null references shop.orders,
  product_id int not null references shop.products,
  quantity int not null,
  unit_price numeric(10,2) not null,
  primary key (order_id, product_id)
);
insert into shop.order_items
select o.id, p.id, 1 + (random() * 3)::int, p.price
from shop.orders o
cross join lateral (select distinct 1 + (random() * 499 + o.id * 0)::int as pid from generate_series(1, 1 + o.id % 4)) r
join shop.products p on p.id = r.pid;

update shop.orders o set total = s.total
from (select order_id, sum(quantity * unit_price) as total from shop.order_items group by order_id) s
where s.order_id = o.id;

create index on shop.orders (customer_id);
create view shop.customer_revenue as
select c.id, c.name, c.country, count(o.id) as orders, sum(o.total) as revenue
from shop.customers c join shop.orders o on o.customer_id = c.id
where o.status in ('delivered', 'shipped')
group by c.id, c.name, c.country;

comment on table shop.orders is 'One row per checkout; total is the sum of its items.';
alter database demo set search_path = shop, public;
analyze;
