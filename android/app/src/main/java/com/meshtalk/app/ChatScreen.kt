package com.meshtalk.app

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import uniffi.mesh_mobile.ReceivedMessage

@Composable
fun ChatScreen(store: MeshStore, modifier: Modifier = Modifier) {
    var draft by remember { mutableStateOf("") }
    val listState = rememberLazyListState()

    LaunchedEffect(store.messages.size) {
        if (store.messages.isNotEmpty()) {
            listState.animateScrollToItem(store.messages.size - 1)
        }
    }

    Column(modifier = modifier.fillMaxSize()) {
        StatusBar(store)

        LazyColumn(
            state = listState,
            modifier = Modifier.fillMaxSize().weight(1f).padding(8.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            items(store.messages) { message -> MessageRow(message) }
        }

        Row(
            modifier = Modifier.fillMaxWidth().padding(8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            OutlinedTextField(
                value = draft,
                onValueChange = { draft = it },
                modifier = Modifier.weight(1f),
                placeholder = { Text("Message") },
            )
            Button(
                onClick = {
                    store.send(draft)
                    draft = ""
                },
                enabled = store.isConnected && draft.isNotBlank(),
            ) {
                Text("Send")
            }
        }
    }
}

@Composable
private fun StatusBar(store: MeshStore) {
    Row(
        modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            text = if (store.isConnected) "connected -- id ${store.nodeId}" else "not connected",
            style = MaterialTheme.typography.bodySmall,
        )
    }
}

@Composable
private fun MessageRow(message: ReceivedMessage) {
    Box(
        modifier = Modifier
            .fillMaxWidth()
            .background(MaterialTheme.colorScheme.surfaceVariant, RoundedCornerShape(8.dp))
            .padding(8.dp),
    ) {
        Text(message.text)
    }
}
