import express from 'express'
import fs from "node:fs"

// load env file
import dotenv from 'dotenv'
dotenv.config()

const port = process.env.PORT || 3000
const filesDir = process.env.FILES_DIR || "public"

if(!fs.existsSync(filesDir)) { // Might be unnecessary
    fs.mkdirSync(filesDir, { recursive: true })
}

const app = express()

app.use(express.static(filesDir))

app.get('/', (req, res) => {
    res.send('Hello World!')
})

app.listen(port, () => {
    console.log(`Listening on port ${port}`)
})
