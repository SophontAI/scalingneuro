-- Optional contributor-provided context about the experiment that produced
-- the synced DICOM folder. Existing uploads remain valid without it.
ALTER TABLE uploads ADD COLUMN experiment_description TEXT;
