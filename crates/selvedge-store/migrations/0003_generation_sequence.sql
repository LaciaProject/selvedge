-- Account generations come from one sequence, so agents can ask for every
-- account that changed after a cursor instead of polling each account.
CREATE SEQUENCE account_generations;
SELECT setval('account_generations', (SELECT coalesce(max(generation), 0) + 1 FROM accounts));
ALTER TABLE accounts ALTER COLUMN generation SET DEFAULT nextval('account_generations');
CREATE INDEX accounts_by_generation ON accounts (generation);
