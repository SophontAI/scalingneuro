# **Scaling Up Neuroimaging Data for Foundation Models**

Modern AI models routinely learn from billions of examples. For language, text scraped from the web enabled foundation models like GPT; for vision, large image datasets like ImageNet were critical. By contrast, neuroimaging data are scarce, fragmented and often locked behind restrictive agreements. Large, well‐curated collections exist (e.g., Human Connectome Project, UK Biobank and OpenNeuro), but their combined hours of fMRI data are tiny compared with web‑scale datasets and access to them is often gated by lengthy applications and usage restrictions. Scaling dataset size is arguably the most critical barrier currently blocking the full potential of neuroAI.

We propose a community‑led effort to build the largest open neuroimaging dataset, spanning functional, diffusion and structural MRI. The goal is to collect tens of thousands of hours of recordings and structural scans each year by automatically sharing unlabeled, privacy-cleared data from MRI facilities. This document outlines the technical plan, legal considerations, funding opportunities, and outreach strategy.

## **Proposed automatic data‑sharing pipeline**

### **Workflow overview**

1. **Automatic data capture** – After each scanning session at a collaborating MRI facility, a local tool identifies all newly acquired, approved MRI outputs—including T1-weighted structural scans and EPI/fMRI series—and prepares them for transfer to a secure cloud storage location. It can watch a scanner’s DICOM output directory or run once against a selected folder, identifying sequences from metadata such as sequence names, acquisition types and vendor-specific tags.

2. **Automatic local defacing** – T1-weighted structural scans and EPI/fMRI series are included in the upload. Before transfer, the script automatically defaces scans in which facial anatomy may be present and verifies both face removal and brain preservation. Scans without facial anatomy in the field of view pass unchanged. Any processing error or failed quality check is quarantined locally rather than uploaded.

3. **Deidentification of metadata** – Subject identifiers (names, medical record numbers, session dates) are removed. Only minimal metadata useful for self‑supervised training (e.g., scanner manufacturer, field strength, TR/TE) are retained.

4. **Unlabeled, research-raw data sharing** – Because the dataset is intended for self-supervised foundation models, no manual labeling is required. Researchers who contribute data get immediate access to the shared dataset via access keys. The archive retains acquisition-level metadata and minimally transformed voxel data while clearly recording any privacy transformation.

5. **Data format and storage** – Source DICOMs remain unchanged at the institution. The open archive stores privacy-cleared NIfTI volumes and minimized metadata sidecars in secure, redundant cloud buckets, with checksums and processing provenance. To reduce size, we are also exploring additional gray-matter focused representations, but the privacy-cleared full volumes remain available for researchers who need them.

6. **Compute integration** – Training jobs can pull data directly from the cloud storage.

### **Implementation details**

* **Portable deployment** – The production experience should be one downloaded launcher for macOS, Windows or Linux, with no manual Python, FSL, Docker or GPU setup. The same tool can run once on a grad student’s laptop or continuously on a scanner-adjacent server. A signed, versioned privacy pack is fetched and cached only if structural processing is needed.

* **Low-resource execution** – CPU-only operation is the baseline. The tool processes one series at a time by default, caps memory use, checkpoints completed work and resumes safely after interruption. Faster machines may opt into bounded parallelism, but the privacy decisions and outputs remain identical.

* **Fail-closed upload** – DICOM conversion, metadata de-identification, structural privacy processing and automated quality checks happen locally. Only a consented output with a complete privacy-pass record can enter the upload queue; unknown sequences and uncertain outputs remain on the source machine.

* **Sequence recognition** – For Siemens scanners, sequence names follow standardized naming conventions, enabling reliable identification of T1, EPI/fMRI and other MRI sequences. 

* **Security** – Data transfer uses encrypted connections (AWS S3 with server‑side encryption).

## **Legal, privacy and ethical considerations**

### **Informed consent and IRB approval**

* **Participant consent** – All scans shared through the pipeline must come from participants who have consented to share their de-identified data for research and open distribution. IRBs at each institution should review the consent language to ensure that automated sharing of unlabeled neuroimaging data is covered. This process is already working successfully at the Princeton Neuroscience Institute; we can provide relevant language from these consent/IRB forms as useful reference.

* **Jurisdiction variability** – Regulations differ across countries and states. In the U.S., HIPAA allows sharing of de‑identified health information; however, local IRBs may impose additional requirements. In Canada and the EU, data protection laws (e.g., Québec Law 25 or GDPR) can limit cross‑border transfer of health data. Collaborators must verify local policies. 

## **Benchmarking and evaluation**

* Related to dataset scaling, we additionally are interested in the development of meaningful benchmarks to evaluate foundation models on brain data. So far, fMRI foundation models have tested only a handful of trait-based tasks (age or gender prediction, control vs clinical group classification, etc.), and results were often saturated or lacked practical relevance. We propose the development of a standardized benchmark framework to evaluate neuroimaging foundation models. This direction likely necessitates its own separate research group from the present discussion surrounding scaling up dataset sharing.

## **Collaboration and outreach strategy**

### **Partnering with imaging centres and consortia**

1. **Leverage existing communities** – The Enigma consortia provide a network of hundreds of imaging centers worldwide. Each consortium focuses on a specific modality or clinical condition and maintains strong social connections among investigators. Engaging Enigma can quickly bootstrap adoption because many centres already collect data and have experience with multi‑site collaborations. Another key partner is OpenNeuro, which hosts tens of thousands of datasets and has preprocessed a large subset using fMRIPrep; partnering with OpenNeuro could provide infrastructure for storage, compute and discoverability. NeuroBagel focuses on discoverability and metadata and might also be a useful resource.

2. **Demonstrate value to contributors** – Facilities that contribute data gain access to the full aggregated dataset, advanced foundation models and compute resources. They can use the data for their own research, boosting their publications without incurring the cost of large‑scale data collection. Contributing also promotes open science and meets funders’ requirements for data sharing.

3. **Pilot sites** – Begin with a small number of pilot sites (e.g., the Norman Lab at Princeton, which already has a prototype pipeline). Use these pilots to refine the script, demonstrate reliability and produce a public example of successful automated sharing. Expand to additional sites gradually.

4. **Technical support** – Provide step‑by‑step installation guides and remote assistance. We can also consider providing monetary incentives to offset the initial work necessary to integrate our data sharing script and amend consent/IRB forms.  
5. **Grant collaborations** – Apply jointly for infrastructure grants (e.g., Brain Canada infrastructure call). Many such grants require matching funds and cross‑institutional partnerships. Foundations like Sloan, Arc or Simons may also support open‑science infrastructure.

## **Funding opportunities**

To sustain a petabyte‑scale neuroimaging resource, dedicated infrastructure funding is essential. Possible sources include:

* **Brain Canada Infrastructure grants** – These support large‑scale neuroinformatics infrastructure but require matching funds from partners. Collaboration with Canadian institutions (e.g., McGill) and philanthropic donors can help meet the match requirement.

* **U.S. National Science Foundation (NSF) Major Research Instrumentation (MRI) grants** – Provide funding for shared scientific infrastructure. Proposals must articulate community benefit and include multiple collaborating institutions.

* **Philanthropic foundations** – The Sloan Foundation, Arc Institute and Simons Foundation have funded open science and computational neuroscience projects. Their grants often favour projects that increase data accessibility and reproducibility.

* **Industry partnerships** – Cloud providers (Google Cloud, AWS, Microsoft Azure) may offer credits or cost‑sharing to support large datasets and compute. LightningAI and other AI companies may supply compute resources as part of corporate social responsibility programs.

## **Next steps**

Refine the automatic upload script, recruit more pilot sites, draft grant proposals, outreach with more partners
